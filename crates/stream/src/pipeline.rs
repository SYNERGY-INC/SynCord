//! Ready-made pipelines: [`Sender`] (source -> scale -> encode -> paced datagrams) and
//! [`Receiver`] (datagrams -> reassemble -> decode -> latest picture), with background threads.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(feature = "capture")]
use crate::capture::{CameraConfig, Capture, ScreenConfig};
use crate::convert::{self, Converter, RawFrame, YuvView};
use crate::frame::{Frame, I420Frame, Output};
use crate::h264::{Decoder, EncoderConfig};
use crate::hwenc::{VideoEncoder, recommended_bitrate};
use crate::packet::{self, CompleteFrame, Reassembler, ReassemblerConfig, ReassemblerStats};
use crate::quality::{CameraQuality, ScreenQuality};
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
    /// ("this is what others see"), updated at most 20 times a second. `None` saves the
    /// conversion.
    pub preview_max_side: Option<u32>,
    /// Use a hardware encoder (Media Foundation / VideoToolbox) when one passes the self-test;
    /// OpenH264 otherwise. Receivers cannot tell the difference.
    pub hardware_encoder: bool,
    /// Lower the bitrate to [`recommended_bitrate`] for the size actually sent (a 1080p screen
    /// shared with a 4K preset does not need 4K's bitrate). The configured bitrate (and
    /// [`Sender::set_bitrate`]) stay the upper limit.
    pub fit_bitrate: bool,
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
            hardware_encoder: true,
            fit_bitrate: false,
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
            hardware_encoder: true,
            fit_bitrate: false,
        }
    }

    /// A screen share preset; pair it with [`ScreenQuality::screen_config`] for the capture.
    pub fn screen_quality(stream_id: u32, quality: ScreenQuality) -> Self {
        let (max_width, max_height) = quality.max_size();
        let fps = quality.fps();
        SenderConfig {
            encoder: EncoderConfig {
                bitrate_bps: quality.bitrate_bps(),
                max_fps: fps as f32,
                // Sharper text up to 1080p30 (~10% less error on text, ~20 ms per moving
                // 1080p frame); beyond that OpenH264's screen mode is too slow, and at 60 fps
                // motion matters more. Hardware encoders ignore it.
                screen_content_tools: fps <= 30 && max_width * max_height <= 1920 * 1080,
                ..EncoderConfig::screen()
            },
            max_width,
            max_height,
            fit_bitrate: true,
            ..SenderConfig::screen(stream_id)
        }
    }

    /// A camera preset; pair it with [`CameraQuality::camera_config`] for the capture.
    pub fn camera_quality(stream_id: u32, quality: CameraQuality) -> Self {
        let (max_width, max_height) = quality.size();
        SenderConfig {
            encoder: EncoderConfig {
                bitrate_bps: quality.bitrate_bps(),
                max_fps: quality.fps() as f32,
                ..EncoderConfig::camera()
            },
            max_width,
            max_height,
            fit_bitrate: true,
            ..SenderConfig::camera(stream_id)
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
    /// The encoder in use: "openh264", "videotoolbox", "nvenc", "amf", "quicksync",
    /// "mf-hardware" (another GPU's Media Foundation encoder) or "mf-software" (Windows' own).
    /// Empty until the first frame was encoded.
    pub encoder: &'static str,
    /// Current target bitrate (after [`SenderConfig::fit_bitrate`]).
    pub bitrate_bps: u32,
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
    pacer: Option<(Arc<PacerShared>, JoinHandle<()>)>,
    #[cfg(feature = "capture")]
    capture: Option<Capture>,
    pattern: Option<(Arc<Mutex<bool>>, JoinHandle<()>)>,
}

impl Sender {
    /// Starts the encoder and pacing threads and the source. `on_packet` is called on the
    /// pacing thread with each datagram (at most `config.max_packet` bytes), in order, each
    /// frame's packets spread over a few milliseconds; keep it quick (hand the bytes to a
    /// channel or socket).
    pub fn start(
        config: SenderConfig,
        source: Source,
        on_packet: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<Sender, String> {
        let encoder = VideoEncoder::new(config.encoder.clone(), config.hardware_encoder)?;
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
                stats: SenderStats { bitrate_bps: config.encoder.bitrate_bps, ..SenderStats::default() },
                error: None,
                preview: None,
            }),
            cv: Condvar::new(),
            max_w: config.max_width.max(2),
            max_h: config.max_height.max(2),
            conv: Mutex::new(Converter::default()),
        });
        let pacer = Arc::new(PacerShared { queue: Mutex::new(PaceQueue::default()), cv: Condvar::new() });
        let p2 = pacer.clone();
        let mut on_packet: PacketSink = Box::new(on_packet);
        let pacer_thread = std::thread::Builder::new()
            .name("celerity-pace".into())
            .spawn(move || pace_loop(&p2, &mut on_packet))
            .map_err(|e| format!("could not start pacing thread: {e}"))?;
        let (sh, p3) = (shared.clone(), pacer.clone());
        let thread = std::thread::Builder::new()
            .name("celerity-encode".into())
            .spawn(move || encode_loop(sh, encoder, config, p3))
            .map_err(|e| format!("could not start encoder thread: {e}"))?;
        let mut sender = Sender {
            shared,
            thread: Some(thread),
            pacer: Some((pacer, pacer_thread)),
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

    /// Changes the target bitrate (bits per second) on the fly; with
    /// [`SenderConfig::fit_bitrate`] it is the upper limit.
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
        if let Some((p, t)) = self.pacer.take() {
            lock(&p.queue).stop = true;
            p.cv.notify_all();
            let _ = t.join();
        }
    }
}

/// Minimum spacing of forced keyframes, so many viewers asking at once cost one keyframe.
const MIN_FORCED_KEY_SPACING_MS: u64 = 300;
/// A quiet source has its last picture re-encoded this often.
const REPEAT_AFTER: Duration = Duration::from_secs(1);

type PacketSink = Box<dyn FnMut(&[u8]) + Send>;

/// The bitrate to give the encoder for frames of `w`x`h`.
fn effective_bitrate(config: &SenderConfig, cap: u32, fps: f32, w: u32, h: u32) -> u32 {
    if config.fit_bitrate && w > 0 { cap.min(recommended_bitrate(config.encoder.content, w, h, fps)) } else { cap }
}

fn encode_loop(sh: Arc<SenderShared>, mut enc: VideoEncoder, config: SenderConfig, pacer: Arc<PacerShared>) {
    let mut frame_no: u32 = 0;
    let start_us = now_us();
    let mut last_key_ms: Option<u64> = None;
    let mut preview_buf = Vec::new();
    let mut preview_serial = 0u64;
    // The local preview is small and only for the user; 20 fps is plenty and saves the
    // conversion of every 1440p60 frame.
    let mut last_preview: Option<Instant> = None;
    // The last encoded picture, re-sent when the source goes quiet (a static screen delivers no
    // frames): keyframe requests still get answered and receivers see the stream is alive.
    let mut last: Option<I420Frame> = None;
    let mut last_encode = Instant::now();
    let mut last_ts_ms = 0u64;
    // Bitrate limit (configured or set by the app) and the frame rate, for `fit_bitrate`.
    let mut cap = config.encoder.bitrate_bps;
    let mut fps = config.encoder.max_fps;
    let mut sized = (0u32, 0u32);
    let mut bufs: Vec<PacedFrame> = Vec::new();
    loop {
        let (frame, force, bitrate, new_fps) = {
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
        if let Some(f) = new_fps {
            fps = f.clamp(1.0, 60.0);
            enc.set_max_fps(fps);
        }
        let size = (frame.width(), frame.height());
        if bitrate.is_some() || new_fps.is_some() || size != sized {
            cap = bitrate.unwrap_or(cap);
            sized = size;
            let target = effective_bitrate(&config, cap, fps, size.0, size.1);
            if target != enc.bitrate() {
                enc.set_bitrate(target);
            }
        }
        if force {
            enc.force_keyframe();
        }
        // Never backwards (a repeated frame can be newer than a late capture).
        let ts_ms = (frame.timestamp_us.saturating_sub(start_us) / 1000).max(last_ts_ms);
        last_ts_ms = ts_ms;
        let target_bps = enc.bitrate();
        let t0 = Instant::now();
        let result = enc.encode(&frame, ts_ms);
        let encode_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let (mut packets, mut bytes, mut encoded, mut keyframes) = (0u64, 0u64, 0u64, 0u64);
        let mut error = None;
        match result {
            Ok(aus) => {
                for au in aus {
                    encoded += 1;
                    keyframes += au.keyframe as u64;
                    if au.keyframe {
                        last_key_ms = Some(au.ts_ms);
                    }
                    let mut pf = bufs.pop().unwrap_or_default();
                    pf.data.clear();
                    pf.ends.clear();
                    let r = packet::packetize(
                        config.stream_id,
                        frame_no,
                        au.keyframe,
                        au.ts_ms as u32,
                        &au.data,
                        config.max_packet,
                        |d| {
                            packets += 1;
                            bytes += d.len() as u64;
                            pf.data.extend_from_slice(d);
                            pf.ends.push(pf.data.len());
                        },
                    );
                    frame_no = frame_no.wrapping_add(1);
                    if let Err(e) = r {
                        error = Some(e);
                    }
                    pf.spread = pace_spread(pf.data.len(), target_bps, fps);
                    pace(&pacer, pf, &mut bufs);
                }
            }
            Err(e) => error = Some(e),
        }
        let name = enc.name();
        let preview_due = last_preview.is_none_or(|t| t.elapsed() >= Duration::from_millis(50));
        let preview = config.preview_max_side.filter(|_| encoded > 0 && preview_due).map(|side| {
            last_preview = Some(Instant::now());
            let (w, h) = convert::fit_side(frame.width(), frame.height(), side);
            convert::to_rgba(&YuvView::from(&frame), w, h, &mut preview_buf);
            preview_serial += 1;
            Frame {
                width: w,
                height: h,
                rgba: Arc::new(preview_buf.clone()),
                i420: None,
                serial: preview_serial,
                timestamp_ms: ts_ms as u32,
            }
        });
        let mut st = lock(&sh.state);
        let s = &mut st.stats;
        if encoded > 0 {
            s.frames_encoded += encoded;
            s.keyframes += keyframes;
            s.packets += packets;
            s.bytes += bytes;
            s.width = frame.width();
            s.height = frame.height();
            s.encoder = name;
        } else if error.is_none() {
            s.frames_skipped += 1;
        }
        s.bitrate_bps = target_bps;
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
// Pacing: a keyframe at 1440p is a few hundred datagrams. Sent back to back they arrive as one
// burst that overflows router and Wi-Fi queues (losing the very frame that repairs loss), so
// each frame's packets are spread over a few milliseconds on a thread of their own; capture and
// encoding never wait for it.

/// Longest a frame's packets are spread over.
const MAX_SPREAD: Duration = Duration::from_millis(8);
/// Packets go out at up to this multiple of the target bitrate (bursts above the average are
/// normal for keyframes and scene changes; what matters is not sending them all at once).
const PACE_FACTOR: f64 = 2.5;

/// One encoded frame's datagrams, back to back in `data`, datagram `i` ending at `ends[i]`.
#[derive(Default)]
struct PacedFrame {
    data: Vec<u8>,
    ends: Vec<usize>,
    spread: Duration,
}

#[derive(Default)]
struct PaceQueue {
    frames: VecDeque<PacedFrame>,
    /// Sent frames, for reuse by the encoder thread.
    done: Vec<PacedFrame>,
    stop: bool,
}

struct PacerShared {
    queue: Mutex<PaceQueue>,
    cv: Condvar,
}

/// How long to spread `bytes` over: at `PACE_FACTOR` times the bitrate, at most `MAX_SPREAD`
/// and half a frame interval.
fn pace_spread(bytes: usize, bitrate: u32, fps: f32) -> Duration {
    let natural = bytes as f64 * 8.0 / (bitrate.max(100_000) as f64 * PACE_FACTOR);
    let limit = MAX_SPREAD.as_secs_f64().min(0.5 / fps.max(1.0) as f64);
    Duration::from_secs_f64(natural.min(limit))
}

/// Queues a frame for the pacer and collects buffers it is done with.
fn pace(p: &PacerShared, frame: PacedFrame, spare: &mut Vec<PacedFrame>) {
    let mut q = lock(&p.queue);
    q.frames.push_back(frame);
    if spare.len() < 4 {
        let n = q.done.len().min(4 - spare.len());
        let at = q.done.len() - n;
        spare.extend(q.done.drain(at..));
    }
    q.done.clear();
    drop(q);
    p.cv.notify_one();
}

fn pace_loop(p: &PacerShared, on_packet: &mut PacketSink) {
    loop {
        let frame = {
            let mut q = lock(&p.queue);
            loop {
                if q.stop {
                    return;
                }
                if let Some(f) = q.frames.pop_front() {
                    break f;
                }
                q = p.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let n = frame.ends.len();
        let t0 = Instant::now();
        let mut start = 0;
        for (i, &end) in frame.ends.iter().enumerate() {
            // Packet i is due at i/n of the spread. Sleeps shorter than a millisecond are not
            // worth it (timers are coarse); those packets just go out together.
            let due = t0 + frame.spread.mul_f64(i as f64 / n as f64);
            let wait = due.saturating_duration_since(Instant::now());
            if wait >= Duration::from_millis(1) {
                let q = lock(&p.queue);
                if q.stop {
                    return;
                }
                // A newer frame waiting means we are behind: send the rest straight away.
                if q.frames.is_empty() {
                    drop(p.cv.wait_timeout(q, wait).unwrap_or_else(|e| e.into_inner()));
                }
            }
            on_packet(&frame.data[start..end]);
            start = end;
        }
        let mut q = lock(&p.queue);
        if q.done.len() < 4 {
            q.done.push(frame);
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
    /// RGBA (default) or I420 planes for GPU conversion.
    pub output: Output,
}

impl ReceiverConfig {
    pub fn new(stream_id: u32) -> Self {
        ReceiverConfig {
            stream_id,
            max_side: 1920,
            reassembler: ReassemblerConfig::default(),
            keyframe_request_interval: Duration::from_millis(500),
            output: Output::Rgba,
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
    /// Average time to decode one frame (plus the output conversion when it was shown).
    pub decode_ms: f32,
    pub reassembler: ReassemblerStats,
}

struct RecvOut {
    frame: Option<Frame>,
    max_side: u32,
    output: Output,
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
                output: config.output,
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

    /// Switches between RGBA and I420 output (applies from the next decoded frame).
    pub fn set_output(&self, output: Output) {
        lock(&self.shared.out).output = output;
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
    let mut rgba: Vec<u8> = Vec::new();
    let mut planes = I420Frame::default();
    let mut conv = Converter::default();
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
        let (max_side, output) = {
            let o = lock(&sh.out);
            (o.max_side, o.output)
        };
        let shown = match dec.decode(&cf.data) {
            Ok(Some(pic)) => {
                if more {
                    // A newer frame is already waiting: decode only (it is a reference), skip
                    // the conversion nobody would see.
                    None
                } else {
                    let (w, h) = match output {
                        Output::Rgba => pic.to_rgba(max_side, &mut rgba),
                        Output::I420 => {
                            let (w, h) = convert::fit_side(pic.width(), pic.height(), max_side);
                            pic.scale_to_i420(w, h, &mut conv, &mut planes);
                            (planes.width(), planes.height())
                        }
                    };
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
            let (pixels, yuv) = match output {
                Output::Rgba => (std::mem::take(&mut rgba), None),
                Output::I420 => (Vec::new(), Some(Arc::new(std::mem::take(&mut planes)))),
            };
            let old = out.frame.replace(Frame {
                width: w,
                height: h,
                rgba: Arc::new(pixels),
                i420: yuv,
                serial,
                timestamp_ms: cf.timestamp_ms,
            });
            drop(out);
            // Reuse the previous buffers if the UI is no longer holding them.
            if let Some(old) = old {
                if let Ok(v) = Arc::try_unwrap(old.rgba)
                    && v.capacity() > 0
                {
                    rgba = v;
                }
                if let Some(Ok(f)) = old.i420.map(Arc::try_unwrap) {
                    planes = f;
                }
            }
        }
    }
}
