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
//! # Layers
//!
//! - [`probe`] and [`poster`] are always available (the server uses these at upload time).
//! - [`Player`] needs the `playback` feature, which adds `cpal` (audio output) and `symphonia`
//!   (AAC decoding).
//!
//! # Memory
//!
//! Only the `moov` header and a compact per-sample index are kept in memory; sample data is read
//! from disk on demand. Frames are converted straight from YUV to RGBA at the requested reduced
//! size. A playing [`Player`] holds at most a handful of scaled frames and ~0.25 s of audio.

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
    /// Display width in pixels (sample aspect ratio applied, rotation metadata ignored).
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
    let (width, height) = movie.video.sps.display_size();
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
        match dec.step(v)? {
            video::Step::Picture(p) => {
                let i = (p.ts as usize).min(track.samples.len() - 1);
                let pts = track.pts(i);
                if pts + track.samples[i].dur as i64 <= 0 && i != first {
                    continue;
                }
                if best.is_none_or(|b| pts < b) {
                    convert::to_rgba(&p, ow, oh, v.sps.matrix, v.sps.full_range, &mut rgba);
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
