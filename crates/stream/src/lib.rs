//! Live video for SynCord voice calls (facecam and screen sharing): capture, H.264 encoding and
//! decoding, and datagram packetization with loss-tolerant reassembly.
//!
//! Everything is pure Rust plus OpenH264, which `openh264-sys2` compiles from bundled C++ source
//! (shared with `syncord-video`). Capture uses the operating system's own APIs directly; no
//! ffmpeg, GStreamer or other media libraries.
//!
//! # Pieces
//!
//! ```text
//! camera / screen --(I420, scaled at capture)--> Sender --encode--> packetize --> on_packet(&[u8])
//!                                                                                  | network
//! Receiver::frame() <--RGBA-- decode <-- reassemble (reorder, dedupe, loss) <-- push_packet()
//!        \--> take_keyframe_request() -> ask the sender -> Sender::force_keyframe()
//! ```
//!
//! # API
//!
//! ```text
//! // Capture (feature "capture", on by default)
//! pub fn list_cameras() -> Result<Vec<DeviceInfo>, String>      // DeviceInfo { id, name }
//! pub fn list_screens() -> Result<Vec<ScreenInfo>, String>      // ScreenInfo { id, name, kind, width, height, primary }
//! pub struct CameraConfig { pub width: u32, pub height: u32, pub fps: u32 }          // default 640x360@30
//! pub struct ScreenConfig { pub max_width: u32, pub max_height: u32, pub fps: u32, pub show_cursor: bool } // 1920x1080@15
//! impl Capture {   // running capture; drop to stop (joins the thread)
//!     pub fn camera(id: &str, cfg: CameraConfig, sink: impl FnMut(&I420Frame) + Send + 'static) -> Result<Capture, String>
//!     pub fn screen(id: &str, cfg: ScreenConfig, sink: impl FnMut(&I420Frame) + Send + 'static) -> Result<Capture, String>
//!     pub fn error(&self) -> Option<String>   // set if it stopped on its own (unplugged...)
//!     pub fn is_running(&self) -> bool
//! }
//!
//! // Pipelines
//! pub enum Source { Camera { id, config }, Screen { id, config }, TestPattern { width, height, fps }, Manual }
//! pub struct SenderConfig { pub stream_id: u32, pub encoder: EncoderConfig, pub max_width: u32,
//!                           pub max_height: u32, pub max_packet: usize, pub preview_max_side: Option<u32> }
//! SenderConfig::camera(stream_id) / SenderConfig::screen(stream_id)   // sensible defaults
//! impl Sender {    // encoder thread; drop to stop
//!     pub fn start(cfg: SenderConfig, source: Source, on_packet: impl FnMut(&[u8]) + Send + 'static) -> Result<Sender, String>
//!     pub fn push_frame(&self, frame: &I420Frame)          // Source::Manual
//!     pub fn push_rgba(&self, width: u32, height: u32, rgba: &[u8])
//!     pub fn force_keyframe(&self)                          // merged if < 300 ms after the last keyframe
//!     pub fn set_bitrate(&self, bps: u32)
//!     pub fn set_max_fps(&self, fps: f32)
//!     pub fn stats(&self) -> SenderStats
//!     pub fn error(&self) -> Option<String>
//!     pub fn preview(&self) -> Option<Frame>                // if preview_max_side is set
//! }
//! pub struct ReceiverConfig { pub stream_id: u32, pub max_side: u32, pub reassembler: ReassemblerConfig,
//!                             pub keyframe_request_interval: Duration }   // ReceiverConfig::new(stream_id)
//! impl Receiver {  // decoder thread; drop to stop
//!     pub fn new(cfg: ReceiverConfig) -> Receiver
//!     pub fn push_packet(&self, datagram: &[u8])            // cheap, call from the network task
//!     pub fn frame(&self) -> Option<Frame>                  // latest picture; re-upload when serial changes
//!     pub fn take_keyframe_request(&self) -> bool           // true at most every 500 ms while broken
//!     pub fn needs_keyframe(&self) -> bool
//!     pub fn set_max_side(&self, max_side: u32)
//!     pub fn stats(&self) -> ReceiverStats
//! }
//!
//! // Building blocks
//! pub struct I420Frame   // new(w, h), width(), height(), y()/u()/v(), planes_mut(), timestamp_us
//! pub struct Frame { pub width: u32, pub height: u32, pub rgba: Arc<Vec<u8>>, pub serial: u64, pub timestamp_ms: u32 }
//! pub struct EncoderConfig { pub content: ContentKind, pub bitrate_bps: u32, pub max_fps: f32,
//!                           pub keyframe_interval: Duration, pub screen_content_tools: bool }
//! impl Encoder { new(cfg), encode(&mut self, &I420Frame, timestamp_ms: u64) -> Result<Option<EncodedFrame>, String>,
//!                force_keyframe(), set_bitrate(bps), set_max_fps(fps), set_keyframe_interval(d) }
//! impl Decoder { new(), decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedPicture>, DecodeError> }
//! impl DecodedPicture { width(), height(), to_rgba(max_side, &mut Vec<u8>) -> (w, h), to_i420(&mut I420Frame) }
//! pub fn packetize(stream_id, frame, keyframe, timestamp_ms, data, max_packet, emit: impl FnMut(&[u8])) -> Result<usize, String>
//! impl PacketHeader { parse(&[u8]) -> Option<(PacketHeader, &[u8])>, write(&self, &mut Vec<u8>) }
//! pub fn stream_id_of(datagram: &[u8]) -> Option<u32>   // for routing
//! impl Reassembler { new(stream_id, cfg), push(&[u8], Instant) -> Result<(), PushError>,
//!                    pop(Instant) -> Option<CompleteFrame>, needs_keyframe(), reset(), stats() }
//! pub fn fit_size(w, h, max_w, max_h) -> (u32, u32)
//! pub fn bgra_to_i420(bgra, w, h, max_w, max_h, out: &mut I420Frame) / rgba_to_i420(...)
//! pub fn render_test_pattern(n: u64, out: &mut I420Frame)
//! pub fn now_us() -> u64
//! ```
//!
//! # Wire format
//!
//! See [`packet`]: an 18-byte header (version, flags, stream id, frame number, fragment index and
//! count, timestamp) plus an opaque payload, at most 1150 bytes per datagram by default.
//!
//! # Keyframes and loss
//!
//! Streams are Constrained Baseline H.264 (no B-frames), one slice per frame (two for frames
//! larger than 720p), SPS/PPS repeated before every keyframe. The receiver releases frames
//! strictly in order; a frame that is still incomplete 200 ms after it (or a later frame)
//! started arriving is declared lost, the picture freezes, and `take_keyframe_request()` turns
//! true until a keyframe arrives. Senders also emit periodic keyframes (camera 4 s, screen 10 s).
//!
//! # Threads and memory
//!
//! - Capture: one thread per capture (plus the OS's own delivery thread on macOS). Frames are
//!   scaled and converted to I420 right away; the capture keeps one I420 frame.
//! - Sender: one encoder thread. A one-slot mailbox sits between capture and encoder: if the
//!   encoder is busy the waiting frame is replaced, so at most 3 I420 frames exist.
//! - Receiver: one decoder thread with a queue of at most 8 compressed frames, one RGBA output
//!   frame (recycled when the UI has dropped it).
//!
//! # Platforms
//!
//! | | Camera | Screen |
//! |---|---|---|
//! | Windows 8+ | Media Foundation source reader (MJPEG decode + scaling by MF) | DXGI desktop duplication, GDI fallback |
//! | macOS 12.3+ | AVFoundation (camera permission) | ScreenCaptureKit, displays and windows (screen recording permission) |
//! | Linux | V4L2 (YUYV, UYVY, NV12, YU12, MJPEG) | X11 (RandR monitors, MIT-SHM, XFixes pointer); Wayland sessions are refused |
//!
//! - `Capture::camera` / `Capture::screen` (and `Sender::start` with a device source) block
//!   until the device streams, which on macOS includes the first-time permission prompt: call
//!   them off the UI thread.
//! - macOS needs `NSCameraUsageDescription` in Info.plist (e.g. "SynCord uses the camera for
//!   video calls."). Screen recording has no Info.plist key; the user grants it in System
//!   Settings > Privacy & Security > Screen Recording and it applies after an app restart (the
//!   error message says so). ScreenCaptureKit is linked, so the app needs macOS 12.3+.
//! - Linux needs no extra system packages (x11rb speaks the X protocol in Rust, V4L2 is plain
//!   `ioctl`, MJPEG is decoded by zune-jpeg).
//! - OpenH264 uses x86 SIMD only when `nasm` is on the PATH at build time (Windows CI: install
//!   it, e.g. `choco install nasm`); without it encoding is plain C and noticeably slower.
//!   aarch64 (Apple Silicon) always gets NEON.
//! - Colour: I420 is treated as BT.709 limited range throughout. Camera YUV is passed through
//!   untouched, so SD webcams that use BT.601 show a slight hue shift.

pub mod packet;

mod convert;
mod frame;
mod h264;
mod pipeline;
mod test_pattern;

#[cfg(feature = "capture")]
mod capture;

#[cfg(feature = "capture")]
pub use capture::{
    CameraConfig, Capture, DeviceInfo, ScreenConfig, ScreenInfo, ScreenKind, list_cameras, list_screens,
};
pub use convert::fit_size;
pub use frame::{ContentKind, Frame, I420Frame};
pub use h264::{DecodeError, DecodedPicture, Decoder, EncodedFrame, Encoder, EncoderConfig};
pub use packet::{
    CompleteFrame, DEFAULT_MAX_PACKET, HEADER_LEN, PacketHeader, PushError, Reassembler, ReassemblerConfig,
    ReassemblerStats, packetize, stream_id_of,
};
pub use pipeline::{Receiver, ReceiverConfig, ReceiverStats, Sender, SenderConfig, SenderStats, Source};
pub use test_pattern::{luma_difference, render_test_pattern};

/// Microseconds on a monotonic clock (since the first call in this process). Capture
/// timestamps ([`I420Frame::timestamp_us`]) use this clock.
pub fn now_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Converts a tightly packed BGRA8 picture to I420, scaled to fit `max_w`x`max_h` (never
/// upscaled, aspect kept). Allocation-free when `out` is reused.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, max_w: u32, max_h: u32, out: &mut I420Frame) {
    rgb_to_i420(bgra, width, height, max_w, max_h, out, true)
}

/// Like [`bgra_to_i420`] for RGBA8 input.
pub fn rgba_to_i420(rgba: &[u8], width: u32, height: u32, max_w: u32, max_h: u32, out: &mut I420Frame) {
    rgb_to_i420(rgba, width, height, max_w, max_h, out, false)
}

fn rgb_to_i420(data: &[u8], width: u32, height: u32, max_w: u32, max_h: u32, out: &mut I420Frame, bgra: bool) {
    let (w, h) = (width as usize, height as usize);
    if data.len() < w * h * 4 {
        return;
    }
    let raw = if bgra {
        convert::RawFrame::Bgra { data, stride: w * 4, width: w, height: h }
    } else {
        convert::RawFrame::Rgba { data, stride: w * 4, width: w, height: h }
    };
    thread_local!(static CONV: std::cell::RefCell<convert::Converter> = Default::default());
    CONV.with(|c| c.borrow_mut().convert(&raw, max_w, max_h, out));
}

#[cfg(test)]
mod tests;
