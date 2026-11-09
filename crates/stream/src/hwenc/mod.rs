//! Encoder selection: a hardware H.264 encoder where the platform has one (Media Foundation on
//! Windows, VideoToolbox on macOS), OpenH264 otherwise or whenever the hardware path misbehaves.
//!
//! Receivers always decode with OpenH264, so a hardware encoder is only used after a self-test:
//! a few test-pattern frames at the real size are encoded and decoded with OpenH264, and the
//! pictures must come back at the right size, without errors and close to the source. Any
//! failure (then or later, mid-stream) falls back to OpenH264 for the rest of the stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::frame::{ContentKind, I420Frame};
use crate::h264::{Decoder, Encoder, EncoderConfig};
use crate::test_pattern::{luma_difference, render_test_pattern};

#[cfg(all(windows, feature = "hardware"))]
mod mf;
#[cfg(all(target_os = "macos", feature = "hardware"))]
mod vt;

/// One encoded access unit (Annex B with 4-byte start codes; SPS and PPS precede every IDR).
#[derive(Debug, Default)]
pub(crate) struct Au {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Timestamp of the input frame this came from.
    pub ts_ms: u64,
}

/// A platform encoder. Created for one frame size; recreated when the size changes.
pub(crate) trait Backend: Send {
    /// Short name for stats ("nvenc", "videotoolbox", ...).
    fn name(&self) -> &'static str;
    /// Encodes `frame` (an IDR if `key`). Appends finished access units to `out`: normally
    /// exactly one, but pipelined hardware may return none now and two later.
    fn encode(&mut self, frame: &I420Frame, ts_ms: u64, key: bool, out: &mut Vec<Au>) -> Result<(), String>;
    /// Target bitrate in bits per second; applies from the next frame.
    fn set_bitrate(&mut self, bps: u32);
    fn set_max_fps(&mut self, fps: f32);
}

/// Creates a backend for a frame size.
pub(crate) type Opener = Box<dyn Fn(&EncoderConfig, u32, u32) -> Result<Box<dyn Backend>, String> + Send>;

/// A way to make a [`Backend`], tried in order of preference.
#[allow(dead_code)] // no candidates on Linux
pub(crate) struct Candidate {
    pub name: &'static str,
    pub open: Opener,
}

/// Hardware (and OS-provided) encoders on this machine, best first.
fn candidates() -> Vec<Candidate> {
    #[allow(unused_mut)]
    let mut list = Vec::new();
    #[cfg(all(windows, feature = "hardware"))]
    list.extend(mf::candidates());
    #[cfg(all(target_os = "macos", feature = "hardware"))]
    list.extend(vt::candidates());
    list
}

/// Prints `msg` to stderr the first time only (the encoder choice is per stream, the message is
/// interesting once per run).
fn log_once(flag: &AtomicBool, msg: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        eprintln!("celerity-stream: {msg}");
    }
}

static LOGGED_FALLBACK: AtomicBool = AtomicBool::new(false);
static LOGGED_RUNTIME: AtomicBool = AtomicBool::new(false);

/// Encodes `frames` test-pattern frames with `backend` and checks that OpenH264 decodes them
/// cleanly, at the right size and close to the source, and that encoding keeps up with `fps`.
pub(crate) fn self_test(backend: &mut dyn Backend, w: u32, h: u32, frames: u64, fps: f32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut dec = Decoder::new()?;
    let mut src = I420Frame::new(w, h);
    let mut sources = Vec::new();
    let mut out = Vec::new();
    let mut decoded = I420Frame::default();
    let (mut pictures, mut first) = (0u64, true);
    let mut worst: f64 = 0.0;
    let mut busy = Duration::ZERO;
    for n in 0..frames + 4 {
        if n < frames {
            render_test_pattern(n * 3, &mut src);
            sources.push(src.clone());
            let t = Instant::now();
            backend.encode(&src, n * 33, n == 0, &mut out)?;
            // The first frames include warm-up (allocations, first keyframe).
            if n >= 2 {
                busy += t.elapsed();
            }
        } else {
            // Give pipelined encoders a moment to hand over what they still hold.
            std::thread::sleep(Duration::from_millis(20));
            backend.encode(&src, n * 33, false, &mut out)?;
        }
        for au in out.drain(..) {
            if first && !au.keyframe {
                return Err("the first frame was not a keyframe".into());
            }
            first = false;
            match dec.decode(&au.data) {
                Ok(Some(pic)) => {
                    if (pic.width(), pic.height()) != (w, h) {
                        return Err(format!("decoded {}x{} instead of {w}x{h}", pic.width(), pic.height()));
                    }
                    pic.to_i420(&mut decoded);
                    let i = (au.ts_ms / 33) as usize;
                    if let Some(s) = sources.get(i.min(frames as usize - 1)) {
                        worst = worst.max(luma_difference(s, &decoded));
                    }
                    pictures += 1;
                }
                Ok(None) => {}
                Err(_) => return Err(format!("OpenH264 could not decode frame {pictures}")),
            }
        }
        if Instant::now() > deadline {
            return Err("too slow".into());
        }
    }
    if pictures + 2 < frames {
        return Err(format!("only {pictures} of {frames} frames came out"));
    }
    // A CPU encoder (the Media Foundation software one) can be too slow for 1440p60; then
    // OpenH264 is the better choice. Hardware takes a few ms.
    let per_frame = busy.as_secs_f64() * 1000.0 / frames.saturating_sub(2).max(1) as f64;
    let budget = 750.0 / fps.clamp(1.0, 60.0) as f64;
    if per_frame > budget {
        return Err(format!("too slow for {fps} fps ({per_frame:.1} ms per frame)"));
    }
    // Generous: the test pattern moves fast and hardware rate control starts conservatively.
    if worst > 14.0 {
        return Err(format!("pictures too far from the source (mean luma error {worst:.1})"));
    }
    Ok(())
}

/// The encoder a [`crate::Sender`] uses: hardware when it passed the self-test, else OpenH264.
/// Keyframe scheduling (interval, forced, size changes) lives here so all backends behave the
/// same.
pub(crate) struct VideoEncoder {
    config: EncoderConfig,
    want_hw: bool,
    /// The hardware path failed once; OpenH264 from now on.
    hw_failed: bool,
    hw: Option<Box<dyn Backend>>,
    /// The candidate that passed the self-test: reused without testing again when the size
    /// changes (a shared window being resized changes it many times a second).
    trusted: Option<usize>,
    sw: Option<Encoder>,
    size: (u32, u32),
    force_key: bool,
    last_key_ms: Option<u64>,
    out: Vec<Au>,
    spare: Vec<Vec<u8>>,
}

impl VideoEncoder {
    pub(crate) fn new(config: EncoderConfig, want_hw: bool) -> Result<VideoEncoder, String> {
        // Creating OpenH264 up front surfaces a broken build immediately (and costs nothing:
        // it allocates on its first frame).
        let sw = Encoder::new(config.clone())?;
        Ok(VideoEncoder {
            config,
            want_hw,
            hw_failed: false,
            hw: None,
            trusted: None,
            sw: Some(sw),
            size: (0, 0),
            force_key: false,
            last_key_ms: None,
            out: Vec::new(),
            spare: Vec::new(),
        })
    }

    /// The encoder in use ("openh264" until the first frame picked something else).
    pub(crate) fn name(&self) -> &'static str {
        self.hw.as_ref().map_or("openh264", |b| b.name())
    }

    pub(crate) fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    pub(crate) fn set_bitrate(&mut self, bps: u32) {
        self.config.bitrate_bps = bps.clamp(50_000, 100_000_000);
        if let Some(hw) = &mut self.hw {
            hw.set_bitrate(self.config.bitrate_bps);
        }
        if let Some(sw) = &mut self.sw {
            sw.set_bitrate(self.config.bitrate_bps);
        }
    }

    pub(crate) fn bitrate(&self) -> u32 {
        self.config.bitrate_bps
    }

    pub(crate) fn set_max_fps(&mut self, fps: f32) {
        self.config.max_fps = fps.clamp(1.0, 60.0);
        if let Some(hw) = &mut self.hw {
            hw.set_max_fps(self.config.max_fps);
        }
        if let Some(sw) = &mut self.sw {
            sw.set_max_fps(self.config.max_fps);
        }
    }

    /// Picks the backend for a new frame size.
    fn reopen(&mut self, w: u32, h: u32) {
        self.size = (w, h);
        self.hw = None;
        self.last_key_ms = None;
        if !self.want_hw || self.hw_failed {
            return;
        }
        let list = candidates();
        if let Some(c) = self.trusted.and_then(|i| list.get(i))
            && let Ok(b) = (c.open)(&self.config, w, h)
        {
            self.hw = Some(b);
            return;
        }
        let mut reasons = Vec::new();
        for (i, c) in list.iter().enumerate() {
            let fps = self.config.max_fps;
            let tested = (c.open)(&self.config, w, h).and_then(|mut b| self_test(b.as_mut(), w, h, 8, fps));
            // A fresh instance for the real stream, so rate control starts clean.
            match tested.and_then(|()| (c.open)(&self.config, w, h)) {
                Ok(b) => {
                    self.hw = Some(b);
                    self.trusted = Some(i);
                    // OpenH264 stays allocated only as the emergency fallback; drop its buffers.
                    self.sw = None;
                    return;
                }
                Err(e) => reasons.push(format!("{}: {e}", c.name)),
            }
        }
        if !reasons.is_empty() {
            log_once(
                &LOGGED_FALLBACK,
                &format!("no usable hardware H.264 encoder ({}), using OpenH264", reasons.join("; ")),
            );
        }
        // Nothing worked at this size; do not retry (and pay the self-test) on every resize.
        self.hw_failed = true;
    }

    /// Encodes one frame. Returns the access units that came out (usually one; none when the
    /// rate controller skipped the frame).
    pub(crate) fn encode(&mut self, frame: &I420Frame, ts_ms: u64) -> Result<&[Au], String> {
        for au in self.out.drain(..) {
            if self.spare.len() < 4 {
                self.spare.push(au.data);
            }
        }
        let (w, h) = (frame.width(), frame.height());
        if self.size != (w, h) {
            self.reopen(w, h);
        }
        if let Some(hw) = &mut self.hw {
            let due = self
                .last_key_ms
                .is_none_or(|k| ts_ms.saturating_sub(k) >= self.config.keyframe_interval.as_millis().max(1) as u64);
            let key = self.force_key || due;
            match hw.encode(frame, ts_ms, key, &mut self.out) {
                Ok(()) => {
                    self.force_key = false;
                    if let Some(k) = self.out.iter().rev().find(|a| a.keyframe) {
                        self.last_key_ms = Some(k.ts_ms);
                    }
                    return Ok(&self.out);
                }
                Err(e) => {
                    log_once(&LOGGED_RUNTIME, &format!("{} failed ({e}); switching to OpenH264", hw.name()));
                    self.hw = None;
                    self.hw_failed = true;
                    self.out.clear();
                    // OpenH264 starts with a keyframe on its own (fresh encoder).
                }
            }
        }
        let force = std::mem::take(&mut self.force_key);
        let mut data = self.spare.pop().unwrap_or_default();
        if self.sw.is_none() {
            self.sw = Some(Encoder::new(self.config.clone())?);
        }
        let sw = self.sw.as_mut().expect("just created");
        if force {
            sw.force_keyframe();
        }
        if let Some(ef) = sw.encode(frame, ts_ms)? {
            data.clear();
            data.extend_from_slice(ef.data);
            self.out.push(Au { data, keyframe: ef.keyframe, ts_ms });
        }
        Ok(&self.out)
    }
}

// ---------------------------------------------------------------------------------------------
// Annex B helpers for platform encoders.

/// NAL unit types we care about.
#[allow(dead_code)]
pub(crate) mod nal {
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
    pub const FILLER: u8 = 12;
}

/// Splits an Annex B stream into NAL units (without start codes).
#[allow(dead_code)]
pub(crate) fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // A 4-byte start code leaves a zero byte at the end of the previous unit; trailing
        // zeros are never part of a NAL unit's payload.
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Remembers the latest SPS/PPS so every IDR can carry them, as receivers that join late (and
/// our fresh-decoder-per-keyframe receiver) need.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct ParamSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

#[allow(dead_code)]
impl ParamSets {
    pub(crate) fn set(&mut self, sps: &[u8], pps: &[u8]) {
        self.sps = sps.to_vec();
        self.pps = pps.to_vec();
    }

    /// Rewrites one access unit given as NAL units into `out` (4-byte start codes): drops
    /// access unit delimiters and filler, records parameter sets and puts them in front of an
    /// IDR that lacks them. Returns whether it is a keyframe.
    pub(crate) fn normalize<'a>(&mut self, units: impl IntoIterator<Item = &'a [u8]>, out: &mut Vec<u8>) -> bool {
        let units: Vec<&[u8]> = units.into_iter().filter(|u| !u.is_empty()).collect();
        let kind = |u: &[u8]| u[0] & 0x1f;
        let idr = units.iter().any(|u| kind(u) == nal::IDR);
        let has_sps = units.iter().any(|u| kind(u) == nal::SPS);
        let has_pps = units.iter().any(|u| kind(u) == nal::PPS);
        for u in &units {
            match kind(u) {
                nal::SPS => self.sps = u.to_vec(),
                nal::PPS => self.pps = u.to_vec(),
                _ => {}
            }
        }
        out.clear();
        if idr && !(has_sps && has_pps) && !self.sps.is_empty() && !self.pps.is_empty() {
            for p in [&self.sps, &self.pps] {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(p);
            }
        }
        for u in units {
            if matches!(kind(u), nal::AUD | nal::FILLER) {
                continue;
            }
            if idr && !(has_sps && has_pps) && matches!(kind(u), nal::SPS | nal::PPS) {
                continue; // already written from the cache
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(u);
        }
        idr
    }
}

/// Recommended bitrate (bits per second) for `width`x`height` at `fps`, interpolated through
/// the presets ([`crate::ScreenQuality`], [`crate::CameraQuality`]): roughly proportional to
/// the pixel rate to the power 0.75.
pub fn recommended_bitrate(kind: ContentKind, width: u32, height: u32, fps: f32) -> u32 {
    let px = width.max(16) as f64 * height.max(16) as f64 * fps.clamp(1.0, 60.0) as f64;
    let (base_bps, base_px) = match kind {
        // 720p30 = 2.5 Mbps, 1080p60 = 7.8, 1440p60 = 12, 4K60 = 22.
        ContentKind::Screen => (2_500_000.0, 1280.0 * 720.0 * 30.0),
        // 720p30 = 1.8 Mbps, 1080p30 = 3.5.
        ContentKind::Camera => (1_800_000.0, 1280.0 * 720.0 * 30.0),
    };
    let exp = if kind == ContentKind::Screen { 0.755 } else { 0.82 };
    (base_bps * (px / base_px).powf(exp)).clamp(150_000.0, 40_000_000.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split_and_normalize() {
        let sps = [0x67, 1, 2, 3];
        let pps = [0x68, 4, 5];
        let idr = [0x65, 9, 9, 0, 0, 0, 9];
        let p = [0x41, 7, 7];
        let aud = [0x09, 0xf0];
        let mut stream = Vec::new();
        for (sc, u) in [(&[0u8, 0, 0, 1][..], &aud[..]), (&[0, 0, 1], &sps), (&[0, 0, 0, 1], &pps), (&[0, 0, 1], &idr)]
        {
            stream.extend_from_slice(sc);
            stream.extend_from_slice(u);
        }
        let units = split_annexb(&stream);
        assert_eq!(units, vec![&aud[..], &sps[..], &pps[..], &idr[..]]);
        let mut ps = ParamSets::default();
        let mut out = Vec::new();
        assert!(ps.normalize(units, &mut out));
        assert_eq!(split_annexb(&out), vec![&sps[..], &pps[..], &idr[..]]);
        // An IDR without parameter sets gets the cached ones; P frames pass through.
        assert!(ps.normalize([&idr[..]], &mut out));
        assert_eq!(split_annexb(&out), vec![&sps[..], &pps[..], &idr[..]]);
        assert!(!ps.normalize([&aud[..], &p[..]], &mut out));
        assert_eq!(out, [0, 0, 0, 1, 0x41, 7, 7]);
    }

    #[test]
    fn bitrates_follow_the_presets() {
        let mbps = |k, w, h, f| recommended_bitrate(k, w, h, f) as f64 / 1e6;
        assert!((mbps(ContentKind::Screen, 1280, 720, 30.0) - 2.5).abs() < 0.01);
        assert!((mbps(ContentKind::Screen, 1920, 1080, 60.0) - 8.0).abs() < 0.5);
        assert!((mbps(ContentKind::Screen, 2560, 1440, 60.0) - 12.0).abs() < 0.5);
        assert!((mbps(ContentKind::Camera, 1920, 1080, 30.0) - 3.5).abs() < 0.2);
    }

    /// OpenH264 against itself, through the same self-test hardware encoders must pass.
    #[test]
    fn self_test_accepts_openh264() {
        struct Soft(Encoder);
        impl Backend for Soft {
            fn name(&self) -> &'static str {
                "openh264"
            }
            fn encode(&mut self, f: &I420Frame, ts: u64, key: bool, out: &mut Vec<Au>) -> Result<(), String> {
                if key {
                    self.0.force_keyframe();
                }
                if let Some(ef) = self.0.encode(f, ts)? {
                    out.push(Au { data: ef.data.to_vec(), keyframe: ef.keyframe, ts_ms: ts });
                }
                Ok(())
            }
            fn set_bitrate(&mut self, bps: u32) {
                self.0.set_bitrate(bps)
            }
            fn set_max_fps(&mut self, fps: f32) {
                self.0.set_max_fps(fps)
            }
        }
        let cfg = EncoderConfig { bitrate_bps: 2_000_000, ..EncoderConfig::camera() };
        let mut b = Soft(Encoder::new(cfg).unwrap());
        // Unoptimised test builds are slow; the timing budget is not what this checks.
        self_test(&mut b, 640, 360, 8, 1.0).unwrap();
    }
}
