//! Inline video for SynCord: probe, poster frames and (with the `playback` feature) a small
//! player with audio, for H.264 video in MP4/MOV files.
//!
//! Everything is pure Rust plus the OpenH264 decoder, which `openh264-sys2` compiles from its
//! bundled C++ source (nasm is used for x86 assembly when present, otherwise plain C++). No
//! ffmpeg, GStreamer or other system media libraries are needed.
//!
//! # Formats
//!
//! - Containers: ISO-BMFF (`.mp4`, `.m4v`, `.mov`). Fragmented MP4 is rejected.
//! - Video: H.264 Constrained Baseline, Main and High profile, 8-bit 4:2:0, progressive, with
//!   B-frames and variable frame rate. HEVC, VP9, AV1, 10-bit / 4:2:2 / 4:4:4 / interlaced H.264
//!   are rejected by [`probe`] with a readable reason.
//! - Audio: AAC-LC, mono or stereo. Other audio (HE-AAC, surround, Opus, MP3, PCM...) does not
//!   make a file unplayable; the video then plays silently and [`Info::has_audio`] is `false`.
//!
//! Rotation metadata (portrait phone videos) is applied: sizes are reported as displayed and frames
//! come out upright.
//!
//! # API
//!
//! Always available (the server uses these at upload time, without the `playback` feature):
//!
//! ```text
//! pub struct Info { pub width: u32, pub height: u32, pub duration_ms: u64, pub has_audio: bool }
//! pub fn probe(path: &Path) -> Result<Info, String>
//! pub fn poster(path: &Path, max_side: u32) -> Result<(Info, image::RgbaImage), String>
//! pub struct Frame { pub width: u32, pub height: u32, pub rgba: Arc<Vec<u8>>, pub serial: u64, pub pts_ms: u64 }
//! ```
//!
//! With the `playback` feature (adds `cpal` for audio output and `symphonia` for AAC decoding):
//!
//! ```text
//! impl Player {                          // Send + Sync; share it as Arc<Player>
//!     pub fn open(path: &Path, max_side: u32) -> Result<Player, String>  // starts paused at 0
//!     pub fn info(&self) -> Info
//!     pub fn play(&self)                 // after the end: restarts from 0
//!     pub fn pause(&self)
//!     pub fn is_playing(&self) -> bool
//!     pub fn seek(&self, ms: u64)        // exact frame, also while paused
//!     pub fn set_volume(&self, v: f32)   // 0.0 ..= 2.0, 1.0 = unchanged
//!     pub fn volume(&self) -> f32
//!     pub fn position_ms(&self) -> u64
//!     pub fn duration_ms(&self) -> u64
//!     pub fn ended(&self) -> bool
//!     pub fn frame(&self) -> Option<Frame>   // frame for the current position
//!     pub fn audio_level(&self) -> f32       // output peak 0..1, for a level meter
//!     pub fn has_audio_output(&self) -> bool // false: no audio track or no device
//! }
//! ```
//!
//! All `Player` methods are cheap (a mutex lock, never decoding), meant to be called from the UI
//! thread every frame; upload `frame()` to a texture when its `serial` changes. Dropping the
//! player stops its threads (joined) and closes the audio stream.
//!
//! # Threads and timing
//!
//! A `Player` runs one video decode thread and, if the file has decodable audio, one audio decode
//! thread that feeds a cpal output stream. The clock follows the audio output while audio plays,
//! and the wall clock otherwise (no audio track, no or broken output device, audio ended or
//! stalled). Frames are reordered to presentation order (B-frames). Late frames are decoded (they
//! may be references) but not converted. Seeking restarts the decoder at the previous keyframe and
//! decodes forward to the target, so a seek costs up to one GOP of decoding.
//!
//! # Memory
//!
//! Only the `moov` header and a compact per-sample index are kept in memory; sample data is read
//! from disk on demand. Frames are converted straight from YUV to RGBA at the requested reduced
//! size. A playing `Player` holds at most a handful of scaled frames and ~0.25 s of audio.

use std::path::Path;

mod convert;
mod demux;
mod h264;
mod video;

#[cfg(feature = "playback")]
mod audio;
#[cfg(feature = "playback")]
mod player;

#[cfg(feature = "playback")]
pub use player::Player;

#[cfg(test)]
mod tests;

/// Basic facts about a playable video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Display width in pixels (sample aspect ratio and rotation metadata applied, so a portrait
    /// phone video is taller than wide).
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Length of the longest playable track.
    pub duration_ms: u64,
    /// `true` if there is an audio track we can decode (AAC-LC).
    pub has_audio: bool,
}

/// A decoded video frame as tightly packed RGBA8 (`width * height * 4` bytes, alpha = 255).
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: std::sync::Arc<Vec<u8>>,
    /// Increases whenever the player hands out a different frame, so a UI can skip re-uploading
    /// a texture it already has.
    pub serial: u64,
    /// Presentation time of this frame.
    pub pts_ms: u64,
}

fn info_of(movie: &demux::Movie) -> Info {
    let (w, h) = movie.video.sps.display_size();
    let (width, height) = if movie.video.rotation % 180 == 90 { (h, w) } else { (w, h) };
    Info { width, height, duration_ms: movie.duration_ms, has_audio: movie.audio.is_some() }
}

/// Fast header-only check: can we play this file, and what are its size and length?
///
/// Reads the container header (no decoding). Returns `Err(reason)` with a short human-readable
/// reason for files we can't play (e.g. "HEVC (H.265) video is not supported, only H.264",
/// "no video track", "not an MP4/MOV file").
pub fn probe(path: &Path) -> Result<Info, String> {
    demux::open(path).map(|m| info_of(&m))
}

/// Decodes the first displayed frame, scaled so its longest side is at most `max_side`
/// (never upscaled), as RGBA8. Meant for chat previews ("poster" images).
///
/// Costs one keyframe decode (plus a few frames for streams with B-frames); memory is bounded by
/// one full-resolution decoder state.
pub fn poster(path: &Path, max_side: u32) -> Result<(Info, image::RgbaImage), String> {
    let movie = demux::open(path)?;
    let info = info_of(&movie);
    let v = &movie.video;
    let track = &v.track;
    let (ow, oh) = convert::scaled_size(info.width, info.height, max_side);

    // The first frame in presentation order that is actually shown (pts + dur > 0).
    let first = (0..track.samples.len())
        .filter(|&i| track.pts(i) + track.samples[i].dur as i64 > 0)
        .min_by_key(|&i| track.pts(i))
        .unwrap_or(0);

    let mut dec = video::VideoDecoder::new(path, v)?;
    dec.restart_at(v.keyframe_before(track.pts_ms(first)))?;
    let mut rgba = Vec::new();
    let mut best: Option<i64> = None; // pts of the frame currently in `rgba`
    loop {
        match dec.step(v, i64::MIN)? {
            video::Step::Picture(p) => {
                let i = (p.ts as usize).min(track.samples.len() - 1);
                let pts = track.pts(i);
                if pts + track.samples[i].dur as i64 <= 0 && i != first {
                    continue;
                }
                if best.is_none_or(|b| pts < b) {
                    convert::to_rgba(&p, ow, oh, v.rotation, v.sps.matrix, v.sps.full_range, &mut rgba);
                    best = Some(pts);
                }
                if i == first {
                    break;
                }
                // Reordering never spans more than a GOP; stop once well past the target.
                if best.is_some() && i > first + 32 {
                    break;
                }
            }
            video::Step::Nothing => {
                if dec.hopeless() {
                    break;
                }
            }
            video::Step::End => break,
        }
    }
    if best.is_none() {
        return Err("could not decode any video frame".into());
    }
    let img = image::RgbaImage::from_raw(ow, oh, rgba).ok_or("internal error: bad frame size")?;
    Ok((info, img))
}
