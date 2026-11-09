//! Ready-made pipelines: [`Sender`] (source -> scale -> encode -> datagrams) and [`Receiver`]
//! (datagrams -> reassemble -> decode -> latest RGBA frame), each with one background thread.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(feature = "capture")]
use crate::capture::{CameraConfig, Capture, ScreenConfig};
use crate::convert::{self, Converter, RawFrame, YuvView};
use crate::frame::{Frame, I420Frame};
use crate::h264::{Decoder, Encoder, EncoderConfig};
use crate::packet::{self, CompleteFrame, Reassembler, ReassemblerConfig, ReassemblerStats};
use crate::{now_us, test_pattern};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Moving average with a fixed weight (exponential), for timing stats.
fn ema(avg: &mut f32, sample: f32) {
    *avg = if *avg == 0.0 { sample } else { *avg * 0.9 + sample * 0.1 };
}

// ---------------------------------------------------------------------------------------------
// Sender

/// Where a [`Sender`] gets its pictures.
#[derive(Clone, Debug)]
pub enum Source {
    #[cfg(feature = "capture")]
    Camera { id: String, config: CameraConfig },
    #[cfg(feature = "capture")]
    Screen { id: String, config: ScreenConfig },
    /// A synthetic moving picture (no device needed).
    TestPattern { width: u32, height: u32, fps: u32 },
    /// Frames are supplied with [`Sender::push_frame`] / [`Sender::push_rgba`].
    Manual,
}

/// Sender settings.
#[derive(Clone, Debug)]
pub struct SenderConfig {
    /// Goes into every packet header so receivers and the relay can tell streams apart.
    pub stream_id: u32,
    pub encoder: EncoderConfig,
    /// Frames larger than this are scaled down (keeping the aspect ratio) before encoding.
    pub max_width: u32,
    pub max_height: u32,
    /// Datagram size limit, header included.
    pub max_packet: usize,
    /// Keep an RGBA copy of the last sent picture with this longest side, for a local preview
    /// ("this is what others see"). `None` saves the conversion.
    pub preview_max_side: Option<u32>,
}

impl SenderConfig {
    /// Facecam defaults: 640x360 max, 700 kbps, 30 fps.
    pub fn camera(stream_id: u32) -> Self {
        SenderConfig {
            stream_id,
            encoder: EncoderConfig::camera(),
            max_width: 640,
            max_height: 360,
            max_packet: packet::DEFAULT_MAX_PACKET,
            preview_max_side: None,
        }
    }

    /// Screen share defaults: 1920x1080 max, 2.5 Mbps, 15 fps.
    pub fn screen(stream_id: u32) -> Self {
        SenderConfig {
            stream_id,
            encoder: EncoderConfig::screen(),
            max_width: 1920,
            max_height: 1080,
            max_packet: packet::DEFAULT_MAX_PACKET,
            preview_max_side: None,
        }
    }
}

/// Sender counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SenderStats {
    /// Frames that reached the sender (after the source's own pacing).
    pub frames_in: u64,
    /// Frames dropped before encoding (over the frame rate, or the encoder was still busy).
    pub frames_dropped: u64,
    pub frames_encoded: u64,
    /// Frames the rate controller skipped to hold the bitrate.
    pub frames_skipped: u64,
    pub keyframes: u64,
    pub packets: u64,
    pub bytes: u64,
    /// Size of the last encoded frame.
    pub width: u32,
    pub height: u32,
    /// Average time spent encoding one frame.
    pub encode_ms: f32,
}

struct SenderState {
    pending: Option<I420Frame>,
    spare: Vec<I420Frame>,
    stop: bool,
    force_key: bool,
    bitrate: Option<u32>,
    max_fps: Option<f32>,
    next_due_us: u64,
    fps: f32,
    stats: SenderStats,
    error: Option<String>,
    preview: Option<Frame>,
}

struct SenderShared {
    state: Mutex<SenderState>,
    cv: Condvar,
    max_w: u32,
    max_h: u32,
    conv: Mutex<Converter>,
}

impl SenderShared {
    /// Hands a frame to the encoder thread (copying or scaling it into a recycled buffer). If the
    /// encoder has not taken the previous frame yet, that one is replaced (dropped).
    fn submit(&self, frame: &I420Frame) {
        let mut buf = {
            let mut st = lock(&self.state);
            if st.stop {
                return;
            }
            st.stats.frames_in += 1;
            let interval = (1_000_000.0 / st.fps.max(1.0)) as u64;
            let ts = frame.timestamp_us;
            if ts + interval / 4 < st.next_due_us {
                st.stats.frames_dropped += 1;
                return;
            }
            st.next_due_us = (st.next_due_us + interval).max(ts);
            st.spare.pop().unwrap_or_default()
        };
        if frame.width() <= self.max_w && frame.height() <= self.max_h {
            buf.copy_from(frame);
        } else {
            let raw = RawFrame::I420 {
                y: frame.y(),
                u: frame.u(),
                v: frame.v(),
                y_stride: frame.width() as usize,
                uv_stride: frame.width() as usize / 2,
                width: frame.width() as usize,
                height: frame.height() as usize,
            };
            lock(&self.conv).convert(&raw, self.max_w, self.max_h, &mut buf);
            buf.timestamp_us = frame.timestamp_us;
        }
        let mut st = lock(&self.state);
        if let Some(old) = st.pending.replace(buf) {
            st.stats.frames_dropped += 1;
            st.spare.push(old);
        }
        drop(st);
        self.cv.notify_one();
    }
}

/// Encodes frames from a source into datagrams on a background thread.
///
/// ```no_run
/// use celerity_stream::{Sender, SenderConfig, Source};
/// let sender = Sender::start(
///     SenderConfig::camera(42),
///     Source::TestPattern { width: 640, height: 360, fps: 30 },
///     |datagram: &[u8]| { /* send over UDP / WebSocket */ },
/// )?;
/// sender.force_keyframe(); // e.g. when a viewer asked for one
/// # Ok::<(), String>(())
/// ```
pub struct Sender {
    shared: Arc<SenderShared>,
    thread: Option<JoinHandle<()>>,
    #[cfg(feature = "capture")]
    capture: Option<Capture>,
    pattern: Option<(Arc<Mutex<bool>>, JoinHandle<()>)>,
}

impl Sender {
    /// Starts the encoder thread and the source. `on_packet` is called on the encoder thread
    /// with each datagram (at most `config.max_packet` bytes), in order; keep it quick (hand the
    /// bytes to a channel or socket).
    pub fn start(
        config: SenderConfig,
        source: Source,
        on_packet: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<Sender, String> {
        let encoder = Encoder::new(config.encoder.clone())?;
        let shared = Arc::new(SenderShared {
            state: Mutex::new(SenderState {
                pending: None,
                spare: Vec::new(),
                stop: false,
                force_key: false,
                bitrate: None,
                max_fps: None,
                next_due_us: 0,
                fps: config.encoder.max_fps,
                stats: SenderStats::default(),
                error: None,
                preview: None,
            }),
            cv: Condvar::new(),
            max_w: config.max_width.max(2),
            max_h: config.max_height.max(2),
            conv: Mutex::new(Converter::default()),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("celerity-encode".into())
            .spawn(move || encode_loop(sh, encoder, config, Box::new(on_packet)))
            .map_err(|e| format!("could not start encoder thread: {e}"))?;
        let mut sender = Sender {
            shared,
            thread: Some(thread),
            #[cfg(feature = "capture")]
            capture: None,
            pattern: None,
        };
        match source {
            #[cfg(feature = "capture")]
            Source::Camera { id, config } => {
                let sh = sender.shared.clone();
                sender.capture = Some(Capture::camera(&id, config, move |f| sh.submit(f))?);
            }
            #[cfg(feature = "capture")]
            Source::Screen { id, config } => {
                let sh = sender.shared.clone();
                sender.capture = Some(Capture::screen(&id, config, move |f| sh.submit(f))?);
            }
            Source::TestPattern { width, height, fps } => {
                let stop = Arc::new(Mutex::new(false));
                let (sh, stop2) = (sender.shared.clone(), stop.clone());
                let t = std::thread::Builder::new()
                    .name("celerity-pattern".into())
                    .spawn(move || {
                        let mut f = I420Frame::new(width, height);
                        let interval = Duration::from_secs_f64(1.0 / fps.clamp(1, 120) as f64);
                        let start = Instant::now();
                        for n in 0u64.. {
                            if *lock(&stop2) {
                                break;
                            }
                            test_pattern::render_test_pattern(n, &mut f);
                            f.timestamp_us = now_us();
                            sh.submit(&f);
                            let next = start + interval * (n as u32 + 1);
                            std::thread::sleep(next.saturating_duration_since(Instant::now()));
                        }
                    })
                    .map_err(|e| format!("could not start test pattern thread: {e}"))?;
                sender.pattern = Some((stop, t));
            }
            Source::Manual => {}
        }
        Ok(sender)
    }

    /// Feeds a frame (for [`Source::Manual`]). Non-blocking; frames beyond the frame rate or while
    /// the encoder is busy are dropped. Set `timestamp_us` (e.g. from [`crate::now_us`]).
    pub fn push_frame(&self, frame: &I420Frame) {
        self.shared.submit(frame);
    }

    /// Feeds an RGBA8 picture (`width * height * 4` bytes), converting it to I420.
    pub fn push_rgba(&self, width: u32, height: u32, rgba: &[u8]) {
        if rgba.len() < (width * height * 4) as usize {
            return;
        }
        let mut f = I420Frame::default();
        let raw =
            RawFrame::Rgba { data: rgba, stride: width as usize * 4, width: width as usize, height: height as usize };
        lock(&self.shared.conv).convert(&raw, self.shared.max_w, self.shared.max_h, &mut f);
        f.timestamp_us = now_us();
        self.shared.submit(&f);
    }

    /// The next frame will be a keyframe (call when a viewer reports loss or joins). Requests
    /// closer than 300 ms to the previous keyframe are merged.
    pub fn force_keyframe(&self) {
        lock(&self.shared.state).force_key = true;
    }

    /// Changes the target bitrate (bits per second) on the fly.
    pub fn set_bitrate(&self, bps: u32) {
        lock(&self.shared.state).bitrate = Some(bps);
    }

    /// Changes the maximum frame rate on the fly.
    pub fn set_max_fps(&self, fps: f32) {
        let mut st = lock(&self.shared.state);
        st.max_fps = Some(fps);
        st.fps = fps.clamp(1.0, 60.0);
    }

    pub fn stats(&self) -> SenderStats {
        lock(&self.shared.state).stats
    }

    /// Last encoder or capture error (the sender keeps running where it can).
    pub fn error(&self) -> Option<String> {
        #[cfg(feature = "capture")]
        if let Some(e) = self.capture.as_ref().and_then(|c| c.error()) {
            return Some(e);
        }
        lock(&self.shared.state).error.clone()
    }

    /// The last sent picture as RGBA, if `preview_max_side` was set.
    pub fn preview(&self) -> Option<Frame> {
        lock(&self.shared.state).preview.clone()
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        #[cfg(feature = "capture")]
        drop(self.capture.take());
        if let Some((stop, t)) = self.pattern.take() {
            *lock(&stop) = true;
            let _ = t.join();
        }
        lock(&self.shared.state).stop = true;
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Minimum spacing of forced keyframes, so many viewers asking at once cost one keyframe.
const MIN_FORCED_KEY_SPACING_MS: u64 = 300;
/// A quiet source has its last picture re-encoded this often.
const REPEAT_AFTER: Duration = Duration::from_secs(1);

type PacketSink = Box<dyn FnMut(&[u8]) + Send>;

fn encode_loop(sh: Arc<SenderShared>, mut enc: Encoder, config: SenderConfig, mut on_packet: PacketSink) {
    let mut frame_no: u32 = 0;
    let start_us = now_us();
    let mut last_key_ms: Option<u64> = None;
    let mut preview_buf = Vec::new();
    let mut preview_serial = 0u64;
    // The last encoded picture, re-sent when the source goes quiet (a static screen delivers no
    // frames): keyframe requests still get answered and receivers see the stream is alive.
    let mut last: Option<I420Frame> = None;
    let mut last_encode = Instant::now();
    let mut last_ts_ms = 0u64;
    loop {
        let (frame, force, bitrate, fps) = {
            let mut st = lock(&sh.state);
            let frame = loop {
                if st.stop {
                    return;
                }
                if let Some(f) = st.pending.take() {
                    break f;
                }
                if let Some(l) = &last {
                    let now_ms = now_us().saturating_sub(start_us) / 1000;
                    let key_due = st.force_key && last_key_ms.is_none_or(|k| now_ms >= k + MIN_FORCED_KEY_SPACING_MS);
                    if key_due || last_encode.elapsed() >= REPEAT_AFTER {
                        let mut f = st.spare.pop().unwrap_or_default();
                        f.copy_from(l);
                        f.timestamp_us = now_us();
                        break f;
                    }
                }
                st = sh.cv.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
            };
            let ts_ms = frame.timestamp_us.saturating_sub(start_us) / 1000;
            let force = st.force_key && last_key_ms.is_none_or(|k| ts_ms >= k + MIN_FORCED_KEY_SPACING_MS);
            if force {
                st.force_key = false;
            }
            (frame, force, st.bitrate.take(), st.max_fps.take())
        };
        if let Some(b) = bitrate {
            enc.set_bitrate(b);
        }
        if let Some(f) = fps {
            enc.set_max_fps(f);
        }
        if force {
            enc.force_keyframe();
        }
        // Never backwards (a repeated frame can be newer than a late capture).
        let ts_ms = (frame.timestamp_us.saturating_sub(start_us) / 1000).max(last_ts_ms);
        last_ts_ms = ts_ms;
        let t0 = Instant::now();
        let result = enc.encode(&frame, ts_ms);
        let encode_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let mut packets = 0u64;
        let mut bytes = 0u64;
        let (mut encoded, mut keyframe, mut error) = (false, false, None);
        match result {
            Ok(Some(ef)) => {
                encoded = true;
                keyframe = ef.keyframe;
                let r = packet::packetize(
                    config.stream_id,
                    frame_no,
                    ef.keyframe,
                    ts_ms as u32,
                    ef.data,
                    config.max_packet,
                    |d| {
                        packets += 1;
                        bytes += d.len() as u64;
                        on_packet(d);
                    },
                );
                frame_no = frame_no.wrapping_add(1);
                if let Err(e) = r {
                    error = Some(e);
                }
            }
            Ok(None) => {}
            Err(e) => error = Some(e),
        }
        if keyframe {
            last_key_ms = Some(ts_ms);
        }
        let preview = config.preview_max_side.filter(|_| encoded).map(|side| {
            let (w, h) = convert::fit_side(frame.width(), frame.height(), side);
            convert::to_rgba(&YuvView::from(&frame), w, h, &mut preview_buf);
            preview_serial += 1;
            Frame {
                width: w,
                height: h,
                rgba: Arc::new(preview_buf.clone()),
                serial: preview_serial,
                timestamp_ms: ts_ms as u32,
            }
        });
        let mut st = lock(&sh.state);
        let s = &mut st.stats;
        if encoded {
            s.frames_encoded += 1;
            s.keyframes += keyframe as u64;
            s.packets += packets;
            s.bytes += bytes;
            s.width = frame.width();
            s.height = frame.height();
        } else if error.is_none() {
            s.frames_skipped += 1;
        }
        ema(&mut s.encode_ms, encode_ms);
        if let Some(e) = error {
            st.error = Some(e);
        }
        if preview.is_some() {
            st.preview = preview;
        }
        last_encode = Instant::now();
        if let Some(old) = last.replace(frame)
            && st.spare.len() < 2
        {
            st.spare.push(old);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Receiver

/// Receiver settings.
#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    /// Only packets with this stream id are accepted.
    pub stream_id: u32,
    /// Decoded frames are scaled so the longest side is at most this (never upscaled).
    pub max_side: u32,
    pub reassembler: ReassemblerConfig,
    /// [`Receiver::take_keyframe_request`] fires at most this often.
    pub keyframe_request_interval: Duration,
}

impl ReceiverConfig {
    pub fn new(stream_id: u32) -> Self {
        ReceiverConfig {
            stream_id,
            max_side: 1920,
            reassembler: ReassemblerConfig::default(),
            keyframe_request_interval: Duration::from_millis(500),
        }
    }
}

/// Receiver counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReceiverStats {
    pub frames_decoded: u64,
    pub decode_errors: u64,
    /// Frames dropped because the decoder fell behind.
    pub frames_overflowed: u64,
    pub keyframe_requests: u64,
    /// Decoded picture size (before scaling for display).
    pub width: u32,
    pub height: u32,
    /// Average time to decode one frame (plus RGBA conversion when it was shown).
    pub decode_ms: f32,
    pub reassembler: ReassemblerStats,
}

struct RecvOut {
    frame: Option<Frame>,
    max_side: u32,
    last_request: Option<Instant>,
    stats: ReceiverStats,
}

struct RecvQueue {
    frames: VecDeque<CompleteFrame>,
    stop: bool,
}

struct RecvShared {
    asm: Mutex<Reassembler>,
    queue: Mutex<RecvQueue>,
    cv: Condvar,
    out: Mutex<RecvOut>,
    request_interval: Duration,
}

/// Frames waiting for the decoder at most; beyond this the decoder is behind and we skip ahead
/// to the next keyframe.
const MAX_QUEUED: usize = 8;

/// Reassembles and decodes one stream on a background thread, keeping only the latest picture.
///
/// ```no_run
/// use celerity_stream::{Receiver, ReceiverConfig};
/// let rx = Receiver::new(ReceiverConfig::new(42));
/// # let datagram = [0u8; 20];
/// rx.push_packet(&datagram);              // from the network
/// if rx.take_keyframe_request() { /* ask the sender for a keyframe */ }
/// if let Some(frame) = rx.frame() { /* upload frame.rgba when frame.serial changed */ }
/// ```
pub struct Receiver {
    shared: Arc<RecvShared>,
    thread: Option<JoinHandle<()>>,
}

impl Receiver {
    pub fn new(config: ReceiverConfig) -> Receiver {
        let shared = Arc::new(RecvShared {
            asm: Mutex::new(Reassembler::new(config.stream_id, config.reassembler.clone())),
            queue: Mutex::new(RecvQueue { frames: VecDeque::new(), stop: false }),
            cv: Condvar::new(),
            out: Mutex::new(RecvOut {
                frame: None,
                max_side: config.max_side.max(2),
                last_request: None,
                stats: ReceiverStats::default(),
            }),
            request_interval: config.keyframe_request_interval,
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new().name("celerity-decode".into()).spawn(move || decode_loop(sh)).ok();
        Receiver { shared, thread }
    }

    /// Adds one datagram from the network. Cheap (reassembly only; decoding happens on the
    /// receiver's thread). Malformed or foreign packets are ignored.
    pub fn push_packet(&self, datagram: &[u8]) {
        let now = Instant::now();
        let mut ready = Vec::new();
        {
            let mut asm = lock(&self.shared.asm);
            if asm.push(datagram, now).is_err() {
                // Still apply timeouts below: a stray packet must not stall anything.
            }
            while let Some(f) = asm.pop(now) {
                ready.push(f);
            }
            if !ready.is_empty() {
                let mut q = lock(&self.shared.queue);
                q.frames.extend(ready.drain(..));
                if q.frames.len() > MAX_QUEUED {
                    // Decoder can't keep up: drop the backlog and resume at a keyframe.
                    let n = q.frames.len() as u64;
                    q.frames.clear();
                    asm.reset();
                    lock(&self.shared.out).stats.frames_overflowed += n;
                }
            }
        }
        self.shared.cv.notify_one();
    }

    /// The most recent decoded picture.
    pub fn frame(&self) -> Option<Frame> {
        lock(&self.shared.out).frame.clone()
    }

    /// `true` while the stream cannot be decoded until the sender sends a keyframe.
    pub fn needs_keyframe(&self) -> bool {
        lock(&self.shared.asm).needs_keyframe()
    }

    /// Returns `true` (at most once per `keyframe_request_interval`) when the sender should be
    /// asked for a keyframe. Poll it from the network task, e.g. every time packets arrive and on
    /// a timer.
    pub fn take_keyframe_request(&self) -> bool {
        if !self.needs_keyframe() {
            return false;
        }
        let mut out = lock(&self.shared.out);
        let now = Instant::now();
        if out.last_request.is_some_and(|t| now.duration_since(t) < self.shared.request_interval) {
            return false;
        }
        out.last_request = Some(now);
        out.stats.keyframe_requests += 1;
        true
    }

    /// Changes the display size limit (applies from the next decoded frame).
    pub fn set_max_side(&self, max_side: u32) {
        lock(&self.shared.out).max_side = max_side.max(2);
    }

    pub fn stats(&self) -> ReceiverStats {
        let asm = lock(&self.shared.asm).stats();
        let mut s = lock(&self.shared.out).stats;
        s.reassembler = asm;
        s
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        lock(&self.shared.queue).stop = true;
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn decode_loop(sh: Arc<RecvShared>) {
    let mut dec = match Decoder::new() {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut serial = 0u64;
    loop {
        let (cf, more) = {
            let mut q = lock(&sh.queue);
            loop {
                if q.stop {
                    return;
                }
                if let Some(f) = q.frames.pop_front() {
                    let more = !q.frames.is_empty();
                    break (f, more);
                }
                q = sh.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        if cf.keyframe {
            // A fresh decoder per keyframe costs little and guarantees no stale state.
            if let Ok(d) = Decoder::new() {
                dec = d;
            }
        }
        let t0 = Instant::now();
        let max_side = lock(&sh.out).max_side;
        let shown = match dec.decode(&cf.data) {
            Ok(Some(pic)) => {
                if more {
                    // A newer frame is already waiting: decode only (it is a reference), skip
                    // the conversion nobody would see.
                    None
                } else {
                    let (w, h) = pic.to_rgba(max_side, &mut buf);
                    Some((w, h, pic.width(), pic.height()))
                }
            }
            Ok(None) => None,
            Err(_) => {
                lock(&sh.asm).reset();
                lock(&sh.queue).frames.clear();
                lock(&sh.out).stats.decode_errors += 1;
                continue;
            }
        };
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        let mut out = lock(&sh.out);
        out.stats.frames_decoded += 1;
        ema(&mut out.stats.decode_ms, ms);
        if let Some((w, h, pw, ph)) = shown {
            serial += 1;
            out.stats.width = pw;
            out.stats.height = ph;
            let rgba = std::mem::take(&mut buf);
            let old = out.frame.replace(Frame {
                width: w,
                height: h,
                rgba: Arc::new(rgba),
                serial,
                timestamp_ms: cf.timestamp_ms,
            });
            drop(out);
            // Reuse the previous buffer if the UI is no longer holding it.
            if let Some(old) = old
                && let Ok(v) = Arc::try_unwrap(old.rgba)
            {
                buf = v;
            }
        }
    }
}
