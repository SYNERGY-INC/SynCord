//! Stream audio: what the computer (or one application) plays, for screen shares with sound.
//!
//! Delivered as 48 kHz interleaved stereo `f32` in 10 ms chunks (480 frames, 960 samples), ready
//! for an Opus encoder. Capture never includes this process's own audio where the platform can
//! exclude it (Windows, macOS), so a voice call is not sent back to the people in it.

use std::time::{Duration, Instant};

#[allow(unused_imports)] // used by the platform stubs
use crate::capture::{Capture, Ready, Stop};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(crate) fn support() -> AudioSupport {
        AudioSupport::default()
    }
    pub(crate) fn run(_: AudioSource, _: Chunker, ready: Ready, _: &Stop) -> Result<(), String> {
        ready(Err("audio capture is not supported on this platform".into()));
        Ok(())
    }
}

/// Output sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Frames (stereo sample pairs) per delivered chunk: 10 ms.
pub const CHUNK_FRAMES: usize = 480;

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// Everything the computer plays except this application (so the call is not re-sent).
    System,
    /// Only the application that owns a shared window: `screen_id` is a [`crate::ScreenInfo::id`]
    /// of kind [`crate::ScreenKind::Window`]. Needs [`AudioSupport::per_app`].
    App { screen_id: String },
}

/// What this machine can capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioSupport {
    /// [`AudioSource::System`] works.
    pub system: bool,
    /// [`AudioSource::App`] works.
    pub per_app: bool,
    /// System capture also contains our own playback (Linux: the call itself would be sent
    /// back). Mute it or warn the user.
    pub includes_own_audio: bool,
}

/// What [`AudioCapture::start`] can do here. Cheap (no device is opened).
///
/// - Windows 10 2004+ (build 19041): system and per-app (process loopback), own audio excluded.
/// - macOS 13+: system and per-app (ScreenCaptureKit; needs the screen recording permission),
///   own audio excluded.
/// - Linux: system only, from the default output's monitor through PulseAudio (or PipeWire's
///   PulseAudio server), loaded at run time; it includes our own audio.
pub fn audio_support() -> AudioSupport {
    platform::support()
}

/// A running audio capture. Dropping it stops the capture and joins its thread.
pub struct AudioCapture {
    inner: Capture,
}

impl AudioCapture {
    /// Starts capturing and calls `sink` with every 10 ms chunk (960 interleaved stereo samples
    /// at 48 kHz) on the capture thread. Silence is delivered as zeros while nothing plays.
    /// Returns once audio flows, or why it cannot (unsupported, no permission, window gone).
    pub fn start(source: AudioSource, sink: impl FnMut(&[f32]) + Send + 'static) -> Result<AudioCapture, String> {
        let chunker = Chunker::new(Box::new(sink));
        let inner = Capture::spawn("celerity-audio", chunker, move |chunker, ready, stop| {
            platform::run(source, chunker, ready, stop)
        })?;
        Ok(AudioCapture { inner })
    }

    /// Set when the capture ended on its own (device gone, the shared app quit, error).
    pub fn error(&self) -> Option<String> {
        self.inner.error()
    }

    /// `false` once the capture thread has ended.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }
}

type AudioSink = Box<dyn FnMut(&[f32]) + Send>;

/// Collects whatever the platform delivers (any rate, any channel count, any packet size) into
/// 48 kHz stereo chunks of exactly [`CHUNK_FRAMES`].
pub(crate) struct Chunker {
    sink: AudioSink,
    buf: Vec<f32>,
    /// Resampler state: position of the next output frame in input frames (fractional), and
    /// the last input frame of the previous packet.
    pos: f64,
    prev: [f32; 2],
    /// When audio was last delivered (for [`Chunker::keep_alive`]).
    last: Option<Instant>,
}

#[allow(dead_code)] // each platform uses some of these
impl Chunker {
    fn new(sink: AudioSink) -> Chunker {
        Chunker { sink, buf: Vec::with_capacity(CHUNK_FRAMES * 4), pos: 0.0, prev: [0.0; 2], last: None }
    }

    /// For platforms that deliver nothing while nothing plays (WASAPI loopback, ScreenCaptureKit):
    /// call regularly; after 100 ms without audio (well beyond their packet spacing of 10-40 ms),
    /// the gap is filled with silence so the consumer's clock keeps running.
    pub(crate) fn keep_alive(&mut self) {
        let now = Instant::now();
        let last = *self.last.get_or_insert(now);
        let gap = now.duration_since(last);
        if gap >= Duration::from_millis(100) {
            let frames = (gap.as_secs_f64() * SAMPLE_RATE as f64) as usize / CHUNK_FRAMES * CHUNK_FRAMES;
            self.buf.resize(self.buf.len() + frames * 2, 0.0);
            self.flush();
            self.last = Some(last + Duration::from_secs_f64(frames as f64 / SAMPLE_RATE as f64));
        }
    }

    fn flush(&mut self) {
        self.last = Some(Instant::now());
        let n = CHUNK_FRAMES * 2;
        let mut start = 0;
        while self.buf.len() - start >= n {
            (self.sink)(&self.buf[start..start + n]);
            start += n;
        }
        self.buf.drain(..start);
    }

    /// 48 kHz interleaved stereo.
    pub(crate) fn push_stereo(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(&samples[..samples.len() & !1]);
        self.flush();
    }

    /// `frames` frames of silence (a gap the platform reported).
    pub(crate) fn push_silence(&mut self, frames: usize) {
        self.buf.resize(self.buf.len() + frames * 2, 0.0);
        self.flush();
    }

    /// Interleaved samples with `channels` channels at `rate` Hz. Mono is duplicated, more than
    /// two channels keep front left/right; other rates are resampled linearly (fine for the
    /// rare device that is not at 48/44.1 kHz; the call codec low-passes anyway).
    pub(crate) fn push_interleaved(&mut self, samples: &[f32], channels: usize, rate: u32) {
        let ch = channels.max(1);
        let frame = |i: usize| -> [f32; 2] {
            let f = &samples[i * ch..i * ch + ch];
            if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] }
        };
        let n = samples.len() / ch;
        if rate == SAMPLE_RATE {
            if ch == 2 {
                self.push_stereo(samples);
            } else {
                for i in 0..n {
                    self.buf.extend_from_slice(&frame(i));
                }
                self.flush();
            }
            return;
        }
        let step = rate as f64 / SAMPLE_RATE as f64;
        // Output frame k lies at input position `pos` (relative to this packet; -1 = prev).
        // Positions from n - 1 on need the next packet's first frame (they become -1..0).
        while self.pos < n as f64 - 1.0 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let a = if i < 0.0 { self.prev } else { frame(i as usize) };
            let b = frame((i + 1.0).max(0.0) as usize);
            self.buf.extend_from_slice(&[a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += step;
        }
        if n > 0 {
            self.prev = frame(n - 1);
            self.pos -= n as f64;
        }
        self.flush();
    }

    /// Planar channels (one slice per channel, equal lengths) at `rate`.
    pub(crate) fn push_planar(&mut self, planes: &[&[f32]], rate: u32) {
        let (Some(l), r) = (planes.first(), planes.get(1)) else { return };
        let r = r.unwrap_or(l);
        let mut tmp = Vec::with_capacity(l.len() * 2);
        for (a, b) in l.iter().zip(r.iter()) {
            tmp.push(*a);
            tmp.push(*b);
        }
        self.push_interleaved(&tmp, 2, rate);
    }

    /// Signed 16-bit interleaved samples.
    pub(crate) fn push_i16(&mut self, samples: &[i16], channels: usize, rate: u32) {
        let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        self.push_interleaved(&f, channels, rate);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    fn collect() -> (Chunker, Arc<Mutex<Vec<Vec<f32>>>>) {
        let got: Arc<Mutex<Vec<Vec<f32>>>> = Arc::default();
        let g = got.clone();
        (Chunker::new(Box::new(move |c: &[f32]| g.lock().unwrap().push(c.to_vec()))), got)
    }

    #[test]
    fn chunks_are_10_ms_of_stereo() {
        let (mut c, got) = collect();
        // Odd packet sizes (WASAPI delivers what it has).
        for n in [100usize, 333, 480, 1000, 7] {
            c.push_stereo(&vec![0.5; n * 2]);
        }
        c.push_silence(1000);
        let total = 100 + 333 + 480 + 1000 + 7 + 1000;
        let chunks = got.lock().unwrap();
        assert_eq!(chunks.len(), total / CHUNK_FRAMES);
        assert!(chunks.iter().all(|c| c.len() == 960));
        assert_eq!(chunks[0][0], 0.5);
    }

    #[test]
    fn resamples_and_maps_channels() {
        let (mut c, got) = collect();
        // One second of a 44.1 kHz mono ramp in odd packets -> ~48000 stereo frames.
        let ramp: Vec<f32> = (0..44_100).map(|i| i as f32 / 44_100.0).collect();
        for p in ramp.chunks(441 * 3 + 17) {
            c.push_interleaved(p, 1, 44_100);
        }
        let chunks = got.lock().unwrap();
        let frames: Vec<[f32; 2]> = chunks.iter().flat_map(|c| c.chunks(2).map(|f| [f[0], f[1]])).collect();
        assert!((frames.len() as i64 - 48_000).abs() <= CHUNK_FRAMES as i64, "{}", frames.len());
        // Left == right, monotonic, no jumps at packet boundaries.
        assert!(frames.iter().all(|f| f[0] == f[1]));
        assert!(frames.windows(2).all(|w| w[1][0] >= w[0][0] && w[1][0] - w[0][0] < 2.0 / 44_100.0));
        drop(chunks);
        let (mut c, got) = collect();
        // 5.1 at 48 kHz keeps front left / right.
        let six: Vec<f32> = (0..480 * 6).map(|i| (i % 6) as f32).collect();
        c.push_interleaved(&six, 6, 48_000);
        assert_eq!(&got.lock().unwrap()[0][..4], &[0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn support_query_does_not_panic() {
        let s = audio_support();
        assert!(!s.per_app || s.system);
    }

    /// Records the default output's monitor while a tone plays. Needs a sound server: on Linux
    /// `pulseaudio -D --exit-idle-time=-1` plus a sink (`pactl load-module module-null-sink`) and
    /// something playing; skipped when audio capture is unsupported.
    #[test]
    fn system_capture_delivers_chunks() {
        if !audio_support().system {
            eprintln!("skipped: no system audio capture here");
            return;
        }
        let got: Arc<Mutex<(usize, f32)>> = Arc::default();
        let g = got.clone();
        let cap = match AudioCapture::start(AudioSource::System, move |c| {
            assert_eq!(c.len(), 960);
            let mut g = g.lock().unwrap();
            g.0 += 1;
            g.1 = c.iter().fold(g.1, |m, s| m.max(s.abs()));
        }) {
            Ok(c) => c,
            Err(e) => {
                // CI machines without a sound device (or permission) end up here.
                eprintln!("skipped: {e}");
                return;
            }
        };
        // The first audio can take a while (an idle PulseAudio sink wakes up slowly).
        let t = std::time::Instant::now();
        while got.lock().unwrap().0 == 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        got.lock().unwrap().0 = 0;
        std::thread::sleep(Duration::from_millis(1000));
        let (n, peak) = *got.lock().unwrap();
        assert!(cap.error().is_none(), "{:?}", cap.error());
        drop(cap);
        // About 100 chunks per second once running.
        assert!((50..=130).contains(&n), "{n} chunks in 1 s");
        eprintln!("{n} audio chunks in 1 s, peak {peak}");
    }
}
