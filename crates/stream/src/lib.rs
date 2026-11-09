//! Live video for Celerity voice calls (facecam and screen sharing): capture (cameras, displays,
//! windows, stream audio), H.264 encoding (hardware where available) and decoding, and paced
//! datagram packetization with loss-tolerant reassembly.
//!
//! Everything is pure Rust plus OpenH264, which `openh264-sys2` compiles from bundled C++ source
//! (shared with `celerity-video`). Capture and hardware encoding use the operating system's own
//! APIs directly; no ffmpeg, GStreamer or other media libraries.
//!
//! # Pieces
//!
//! ```text
//! camera / screen / window --(I420, scaled at capture)--> Sender --encode (hardware or OpenH264)
//!     --> packetize --> pacing thread --> on_packet(&[u8])                         | network
//! Receiver::frame() <--RGBA or I420-- decode (OpenH264) <-- reassemble (reorder, dedupe, loss)
//!        \--> take_keyframe_request() -> ask the sender -> Sender::force_keyframe()
//! AudioCapture --(48 kHz stereo f32, 10 ms chunks)--> sink   (the client encodes it with Opus)
//! ```
//!
//! # API
//!
//! ```text
//! // Capture (feature "capture", on by default)
//! pub fn list_cameras() -> Result<Vec<DeviceInfo>, String>      // DeviceInfo { id, name }
//! pub fn list_screens() -> Result<Vec<ScreenInfo>, String>      // displays first, then application windows
//! pub struct ScreenInfo { pub id: String, pub name: String, pub kind: ScreenKind /* Display | Window */,
//!                         pub width: u32, pub height: u32, pub primary: bool }   // window ids start with "window:"
//! pub struct CameraConfig { pub width: u32, pub height: u32, pub fps: u32 }          // default 640x360@30
//! pub struct ScreenConfig { pub max_width: u32, pub max_height: u32, pub fps: u32, pub show_cursor: bool } // 1920x1080@15
//! impl Capture {   // running capture; drop to stop (joins the thread)
//!     pub fn camera(id: &str, cfg: CameraConfig, sink: impl FnMut(&I420Frame) + Send + 'static) -> Result<Capture, String>
//!     pub fn screen(id: &str, cfg: ScreenConfig, sink: impl FnMut(&I420Frame) + Send + 'static) -> Result<Capture, String>
//!     pub fn error(&self) -> Option<String>   // set if it stopped on its own (unplugged, "the window was closed"...)
//!     pub fn is_running(&self) -> bool
//! }
//!
//! // Stream audio (feature "capture")
//! pub enum AudioSource { System /* everything except this app */, App { screen_id: String } /* a shared window's app */ }
//! pub struct AudioSupport { pub system: bool, pub per_app: bool, pub includes_own_audio: bool }
//! pub fn audio_support() -> AudioSupport                // cheap; decide what to offer in the UI
//! impl AudioCapture {  // drop to stop
//!     pub fn start(source: AudioSource, sink: impl FnMut(&[f32]) + Send + 'static) -> Result<AudioCapture, String>
//!     pub fn error(&self) -> Option<String>
//!     pub fn is_running(&self) -> bool
//! }   // sink gets 960 samples per call: 480 frames of interleaved stereo at 48 kHz (10 ms); silence as zeros
//!
//! // Quality presets
//! pub enum ScreenQuality { Res720p30, Res1080p30, Res1080p60, Res1440p60 /* default */, Source30, Source60 /* up to 4K */ }
//! pub enum CameraQuality { Res480p, Res720p /* default */, Res1080p }
//! impl ScreenQuality { ALL, max_size(), fps(), bitrate_bps(), label(), screen_config(show_cursor) -> ScreenConfig }
//! impl CameraQuality { ALL, size(), fps(), bitrate_bps(), label(), camera_config() -> CameraConfig }
//! pub fn recommended_bitrate(kind: ContentKind, width, height, fps: f32) -> u32
//!
//! // Pipelines
//! pub enum Source { Camera { id, config }, Screen { id, config }, TestPattern { width, height, fps }, Manual }
//! pub struct SenderConfig { pub stream_id: u32, pub encoder: EncoderConfig, pub max_width: u32,
//!                           pub max_height: u32, pub max_packet: usize, pub preview_max_side: Option<u32>,
//!                           pub hardware_encoder: bool /* default true */, pub fit_bitrate: bool }
//! SenderConfig::camera(stream_id) / SenderConfig::screen(stream_id)   // legacy defaults (640x360, 1080p15)
//! SenderConfig::screen_quality(stream_id, ScreenQuality) / SenderConfig::camera_quality(stream_id, CameraQuality)
//! impl Sender {    // encoder + pacing threads; drop to stop
//!     pub fn start(cfg: SenderConfig, source: Source, on_packet: impl FnMut(&[u8]) + Send + 'static) -> Result<Sender, String>
//!     pub fn push_frame(&self, frame: &I420Frame)          // Source::Manual
//!     pub fn push_rgba(&self, width: u32, height: u32, rgba: &[u8])
//!     pub fn force_keyframe(&self)                          // merged if < 300 ms after the last keyframe
//!     pub fn set_bitrate(&self, bps: u32)                   // the cap when fit_bitrate is on
//!     pub fn set_max_fps(&self, fps: f32)
//!     pub fn stats(&self) -> SenderStats                    // ..., encoder: &'static str, bitrate_bps: u32
//!     pub fn error(&self) -> Option<String>
//!     pub fn preview(&self) -> Option<Frame>                // RGBA, if preview_max_side is set
//! }
//! pub struct ReceiverConfig { pub stream_id: u32, pub max_side: u32, pub reassembler: ReassemblerConfig,
//!                             pub keyframe_request_interval: Duration, pub output: Output }   // ReceiverConfig::new(stream_id)
//! pub enum Output { Rgba /* default */, I420 /* planes for a GPU shader */ }
//! impl Receiver {  // decoder thread; drop to stop
//!     pub fn new(cfg: ReceiverConfig) -> Receiver
//!     pub fn push_packet(&self, datagram: &[u8])            // cheap, call from the network task
//!     pub fn frame(&self) -> Option<Frame>                  // latest picture; re-upload when serial changes
//!     pub fn take_keyframe_request(&self) -> bool           // true at most every 500 ms while broken
//!     pub fn needs_keyframe(&self) -> bool
//!     pub fn set_max_side(&self, max_side: u32)
//!     pub fn set_output(&self, output: Output)
//!     pub fn stats(&self) -> ReceiverStats
//! }
//!
//! // Building blocks
//! pub struct I420Frame   // new(w, h), width(), height(), y()/u()/v(), y_stride(), uv_stride(), planes_mut(), timestamp_us
//! pub struct Frame { pub width: u32, pub height: u32, pub rgba: Arc<Vec<u8>> /* empty with Output::I420 */,
//!                    pub i420: Option<Arc<I420Frame>> /* Output::I420 */, pub serial: u64, pub timestamp_ms: u32 }
//! impl Frame { pixels(&self) -> Pixels }   // Pixels::Rgba(&[u8]) | Pixels::I420 { y, u, v, y_stride, uv_stride }
//! pub struct EncoderConfig { pub content: ContentKind, pub bitrate_bps: u32, pub max_fps: f32,
//!                           pub keyframe_interval: Duration, pub screen_content_tools: bool }
//! impl Encoder { new(cfg), encode(&mut self, &I420Frame, timestamp_ms: u64) -> Result<Option<EncodedFrame>, String>,
//!                force_keyframe(), set_bitrate(bps), set_max_fps(fps), set_keyframe_interval(d) }   // OpenH264
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
//! Unchanged since 0.6: older receivers decode streams from newer senders.
//!
//! # Encoders
//!
//! Receivers always decode with OpenH264. Senders use, in order of preference:
//!
//! - Windows: the GPU's Media Foundation encoder (NVENC, AMF, Quick Sync; asynchronous MFTs are
//!   driven through their event queue), then Windows' own software MFT, then OpenH264.
//! - macOS: VideoToolbox (hardware, low-latency rate control first), then OpenH264.
//! - Linux: OpenH264.
//!
//! A platform encoder is only used after a self-test at the stream's real size: a few test
//! frames must decode with OpenH264 at the right size, without errors, close to the source and
//! fast enough for the frame rate. Any failure, then or mid-stream, falls back to OpenH264 for
//! the rest of the stream (printed once to stderr). `SenderStats::encoder` says which one runs.
//! Hardware streams are High profile without B-frames (CABAC, 8x8 transforms; OpenH264 decodes
//! both); OpenH264 streams are Constrained Baseline.
//!
//! # Keyframes, loss and pacing
//!
//! No B-frames, SPS/PPS before every keyframe. OpenH264 uses one slice per thread: one up to
//! 720p30, two up to 1080p30, four beyond. The receiver releases frames strictly in order; a
//! frame that is still incomplete 200 ms after it (or a later frame) started arriving is declared
//! lost, the picture freezes, and `take_keyframe_request()` turns true until a keyframe arrives.
//! Senders also emit periodic keyframes (camera 4 s, screen 10 s). Each frame's datagrams are
//! sent over up to 8 ms (at 2.5x the bitrate, at most half a frame interval) by a pacing thread,
//! so a keyframe does not hit the network as one burst; capture and encoding never wait for it.
//!
//! # Threads and memory
//!
//! - Capture: one thread per capture (plus the OS's own delivery thread on macOS). Frames are
//!   scaled and converted to I420 right away; the capture keeps one I420 frame.
//! - Sender: an encoder thread and a pacing thread. A one-slot mailbox sits between capture and
//!   encoder: if the encoder is busy the waiting frame is replaced, so at most 3 I420 frames
//!   exist (plus one NV12 copy for a platform encoder).
//! - Receiver: one decoder thread with a queue of at most 8 compressed frames, one RGBA (or I420)
//!   output frame (recycled when the UI has dropped it).
//! - Audio: one thread per capture (plus the OS's delivery thread on macOS).
//!
//! # Platforms
//!
//! | | Camera | Screen and windows | Stream audio |
//! |---|---|---|---|
//! | Windows 10+ | Media Foundation source reader (MJPEG decode + scaling by MF) | displays: DXGI desktop duplication, GDI fallback; windows: Windows.Graphics.Capture (1903+) | process loopback (2004+): all but us, or one app's process tree |
//! | macOS 12.3+ | AVFoundation (camera permission) | ScreenCaptureKit, displays and windows (screen recording permission) | ScreenCaptureKit (13+): all but us, or one app |
//! | Linux | V4L2 (YUYV, UYVY, NV12, YU12, MJPEG) | X11 (RandR monitors, MIT-SHM, XFixes pointer; windows via XComposite); Wayland sessions are refused | PulseAudio/PipeWire monitor of the default output (includes our own audio) |
//!
//! - `Capture::camera` / `Capture::screen` / `AudioCapture::start` (and `Sender::start` with a
//!   device source) block until the device streams, which on macOS includes the first-time
//!   permission prompt: call them off the UI thread. `Sender::start` also runs the encoder
//!   self-test on its first frame (tens of ms, once per stream and size).
//! - Window capture: a minimised window sends nothing (the sender repeats its last picture once a
//!   second), a resized one continues at its new size, a closed one stops the capture with
//!   "the window was closed". Windows has no yellow border on Windows 11 (`IsBorderRequired`).
//! - macOS needs `NSCameraUsageDescription` in Info.plist (e.g. "Celerity uses the camera for
//!   video calls."). Screen recording (also needed for stream audio) has no Info.plist key; the
//!   user grants it in System Settings > Privacy & Security > Screen Recording and it applies
//!   after an app restart (the error message says so). ScreenCaptureKit is linked, so the app
//!   needs macOS 12.3+; audio capture is detected at run time.
//! - Linux needs no extra system packages (x11rb speaks the X protocol in Rust, V4L2 is plain
//!   `ioctl`, MJPEG is decoded by zune-jpeg, `libpulse-simple.so.0` is opened at run time).
//! - OpenH264 uses x86 SIMD only when `nasm` is on the PATH at build time (Windows CI: install
//!   it, e.g. `choco install nasm`); without it encoding is plain C and noticeably slower.
//!   aarch64 (Apple Silicon) always gets NEON. Pixel conversions use AVX2 when the CPU has it.
//! - Colour: I420 is treated as BT.709 limited range throughout. Camera YUV is passed through
//!   untouched, so SD webcams that use BT.601 show a slight hue shift.

pub mod packet;

mod convert;
mod frame;
mod h264;
mod hwenc;
mod pipeline;
mod quality;
mod test_pattern;

#[cfg(feature = "capture")]
mod audio;
#[cfg(feature = "capture")]
mod capture;

#[cfg(feature = "capture")]
pub use audio::{AudioCapture, AudioSource, AudioSupport, audio_support};
#[cfg(feature = "capture")]
pub use capture::{
    CameraConfig, Capture, DeviceInfo, ScreenConfig, ScreenInfo, ScreenKind, list_cameras, list_screens,
};
pub use convert::fit_size;
pub use frame::{ContentKind, Frame, I420Frame, Output, Pixels};
pub use h264::{DecodeError, DecodedPicture, Decoder, EncodedFrame, Encoder, EncoderConfig};
pub use hwenc::recommended_bitrate;
pub use packet::{
    CompleteFrame, DEFAULT_MAX_PACKET, HEADER_LEN, PacketHeader, PushError, Reassembler, ReassemblerConfig,
    ReassemblerStats, packetize, stream_id_of,
};
pub use pipeline::{Receiver, ReceiverConfig, ReceiverStats, Sender, SenderConfig, SenderStats, Source};
pub use quality::{CameraQuality, ScreenQuality};
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
