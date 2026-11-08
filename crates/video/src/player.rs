//! Background-threaded player: one thread decodes video into a tiny queue of scaled RGBA frames,
//! one thread decodes audio into a ~0.25 s ring buffer that the output callback drains.
//!
//! Timing: while audio is playing, the output callback stamps the media time it is currently
//! playing and the clock extrapolates from that stamp (capped, so video waits if audio stalls).
//! Without audio (no track, unsupported codec, no output device, or audio already finished) the
//! clock is the wall clock.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{AacDecoder, Resampler, remix};
use crate::demux::{self, Movie, SampleReader};
use crate::video::{Step, VideoDecoder};
use crate::{Frame, Info, convert, info_of};

/// Decoded frames buffered ahead of the clock.
const VIDEO_QUEUE: usize = 3;
/// Audio buffered ahead of the output, in milliseconds.
const AUDIO_BUFFER_MS: usize = 250;
/// How far the clock may run past the last audio stamp before waiting for audio.
const AUDIO_CLOCK_CAP: Duration = Duration::from_millis(250);
/// No audio delivered for this long while playing (stuck device, decoder not keeping up): the
/// clock continues on the wall clock until audio resumes.
const AUDIO_STALL: Duration = Duration::from_millis(1000);
/// While decoding can't keep up, late frames are normally dropped; but once the shown frame is
/// this stale, a late frame is shown anyway (a low frame rate instead of a frozen picture).
const MAX_STALE_MS: f64 = 250.0;
/// Poll interval of the worker threads while idle.
const TICK: Duration = Duration::from_millis(20);

struct QFrame {
    pts_ms: i64,
    rgba: Arc<Vec<u8>>,
}

struct AudioState {
    /// Audio output is running and drives the clock while it has data.
    active: bool,
    /// The last output callback had real samples (clock extrapolation is capped).
    driving: bool,
    /// The decoder reached the end of the track (for the current seek generation).
    done: bool,
    /// Interleaved f32 at the device rate / channel count.
    ring: VecDeque<f32>,
    /// Media time of `ring.front()`.
    ring_ms: f64,
    /// `ring_ms` has been set for the current seek generation.
    ring_started: bool,
    rate: u32,
    channels: usize,
}

struct State {
    shutdown: bool,
    playing: bool,
    ended: bool,
    /// Incremented by every seek; workers restart when it changes.
    epoch: u64,
    seek_ms: u64,
    // Video
    queue: VecDeque<QFrame>,
    video_eof: bool,
    current: Option<(Frame, u64)>,
    serial: u64,
    /// Spare frame buffers, so steady playback doesn't allocate (and page-fault) per frame.
    pool: Vec<Vec<u8>>,
    // Clock: position = base_ms + (now - at) while playing.
    base_ms: f64,
    at: Instant,
    last_pos: f64,
    audio: AudioState,
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
    duration_ms: u64,
    volume: AtomicU32,
    level: AtomicU32,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl State {
    fn pos(&self, now: Instant, duration: u64) -> f64 {
        let mut p = self.base_ms;
        if self.playing {
            let mut e = now.saturating_duration_since(self.at);
            if self.audio.active && self.audio.driving {
                e = e.min(AUDIO_CLOCK_CAP);
            }
            p += e.as_secs_f64() * 1000.0;
        }
        p.max(self.last_pos).clamp(0.0, duration as f64)
    }

    /// Advances the reported position and detects the end of playback.
    fn tick(&mut self, now: Instant, duration: u64) -> f64 {
        let p = self.pos(now, duration);
        if self.playing
            && self.audio.active
            && self.audio.driving
            && now.saturating_duration_since(self.at) > AUDIO_STALL
        {
            self.audio.driving = false;
            self.set_clock(p, now);
        }
        self.last_pos = p;
        if self.playing && p >= duration as f64 {
            // While playing, `driving == false` means the wall clock has taken over from audio.
            let audio_finished =
                !self.audio.active || !self.audio.driving || (self.audio.done && self.audio.ring.is_empty());
            let frames_shown = self.queue.back().is_none_or(|f| f.pts_ms as f64 <= p);
            if self.video_eof && frames_shown && audio_finished {
                self.ended = true;
                self.playing = false;
                self.base_ms = duration as f64;
                self.at = now;
            }
        }
        p
    }

    /// Keeps a no longer needed frame buffer for reuse (if nobody else holds it).
    fn recycle(&mut self, rgba: Arc<Vec<u8>>) {
        if let Ok(v) = Arc::try_unwrap(rgba)
            && self.pool.len() < VIDEO_QUEUE + 1
        {
            self.pool.push(v);
        }
    }

    fn set_clock(&mut self, ms: f64, now: Instant) {
        self.base_ms = ms;
        self.at = now;
        self.last_pos = ms;
    }
}

/// Which audio output a player uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioOut {
    /// The default output device via cpal (silently disabled if it can't be opened).
    Device,
    /// A fake real-time sink (tests on machines without a sound card).
    #[cfg_attr(not(test), allow(dead_code))]
    Null,
    /// A fake device that opens but never asks for samples (tests the stall fallback).
    #[cfg_attr(not(test), allow(dead_code))]
    Stuck,
}

/// A video player. Starts paused at 0 with the first frame decoded.
///
/// All methods take `&self` and are cheap (a mutex lock, no decoding), so call them from the UI
/// thread every frame. `Player` is `Send + Sync`; share it with `Arc<Player>`. Dropping it stops
/// the decoding threads promptly (joined in `Drop`) and closes the audio stream.
pub struct Player {
    shared: Arc<Shared>,
    info: Info,
    /// Size of the frames handed out (fixed per player).
    frame_size: (u32, u32),
    threads: Vec<JoinHandle<()>>,
}

impl Player {
    /// Opens `path` for playback; frames are scaled so the longest side is at most `max_side`.
    ///
    /// Fails like [`probe`](crate::probe) for unsupported files. A missing or broken audio device
    /// is not an error: the video then plays silently, timed by the wall clock.
    pub fn open(path: &Path, max_side: u32) -> Result<Player, String> {
        Self::open_with(path, max_side, AudioOut::Device)
    }

    fn open_with(path: &Path, max_side: u32, out: AudioOut) -> Result<Player, String> {
        let movie = Arc::new(demux::open(path)?);
        let info = info_of(&movie);
        let now = Instant::now();
        let shared = Arc::new(Shared {
            st: Mutex::new(State {
                shutdown: false,
                playing: false,
                ended: false,
                epoch: 0,
                seek_ms: 0,
                queue: VecDeque::new(),
                video_eof: false,
                current: None,
                serial: 0,
                pool: Vec::new(),
                base_ms: 0.0,
                at: now,
                last_pos: 0.0,
                audio: AudioState {
                    active: false,
                    driving: false,
                    done: false,
                    ring: VecDeque::new(),
                    ring_ms: 0.0,
                    ring_started: false,
                    rate: 0,
                    channels: 0,
                },
            }),
            cv: Condvar::new(),
            duration_ms: info.duration_ms,
            volume: AtomicU32::new(1.0f32.to_bits()),
            level: AtomicU32::new(0),
        });
        let path: PathBuf = path.to_path_buf();
        let frame_size = convert::scaled_size(info.width, info.height, max_side);
        // Built before spawning so that if a spawn fails, dropping it stops the started threads.
        let mut player = Player { shared: shared.clone(), info, frame_size, threads: Vec::new() };
        {
            let (sh, m, p) = (shared.clone(), movie.clone(), path.clone());
            let (w, h) = frame_size;
            player.threads.push(
                std::thread::Builder::new()
                    .name("video-decode".into())
                    .spawn(move || video_thread(&sh, &m, &p, w, h))
                    .map_err(|e| format!("can't start video thread: {e}"))?,
            );
        }
        if movie.audio.is_some() {
            let (sh, m) = (shared, movie.clone());
            player.threads.push(
                std::thread::Builder::new()
                    .name("audio-decode".into())
                    .spawn(move || audio_thread(&sh, &m, &path, out))
                    .map_err(|e| format!("can't start audio thread: {e}"))?,
            );
        }
        Ok(player)
    }

    /// Size, duration and whether the file has (decodable) audio.
    pub fn info(&self) -> Info {
        self.info
    }

    /// Starts or resumes playback. After the end, starts again from the beginning.
    pub fn play(&self) {
        let mut st = self.shared.lock();
        if st.ended {
            drop(st);
            self.seek(0);
            st = self.shared.lock();
        }
        if !st.playing {
            let now = Instant::now();
            let p = st.pos(now, self.shared.duration_ms);
            st.playing = true;
            st.set_clock(p, now);
            // Wait for audio to resume rather than run ahead of it.
            st.audio.driving = st.audio.active && !st.audio.done;
        }
        self.shared.cv.notify_all();
    }

    pub fn pause(&self) {
        let mut st = self.shared.lock();
        if st.playing {
            let now = Instant::now();
            let p = st.tick(now, self.shared.duration_ms);
            st.playing = false;
            st.set_clock(p, now);
        }
        self.shared.level.store(0, Ordering::Relaxed);
        self.shared.cv.notify_all();
    }

    pub fn is_playing(&self) -> bool {
        let mut st = self.shared.lock();
        st.tick(Instant::now(), self.shared.duration_ms);
        st.playing
    }

    /// Jumps to `ms` (clamped to the duration). Decoding restarts at the nearest earlier keyframe
    /// and skips forward, so the shown frame is the one at `ms` (not the keyframe). Works while
    /// paused too: the frame at the new position is decoded and shown.
    pub fn seek(&self, ms: u64) {
        let mut st = self.shared.lock();
        let ms = ms.min(self.shared.duration_ms);
        st.epoch += 1;
        st.seek_ms = ms;
        st.ended = false;
        while let Some(f) = st.queue.pop_front() {
            st.recycle(f.rgba);
        }
        st.video_eof = false;
        st.set_clock(ms as f64, Instant::now());
        let a = &mut st.audio;
        a.ring.clear();
        a.ring_started = false;
        a.done = false;
        a.driving = a.active;
        self.shared.cv.notify_all();
    }

    /// Output volume, 0.0 (mute) to 2.0 (double); 1.0 is unchanged.
    pub fn set_volume(&self, v: f32) {
        let v = if v.is_finite() { v.clamp(0.0, 2.0) } else { 1.0 };
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    pub fn position_ms(&self) -> u64 {
        let mut st = self.shared.lock();
        st.tick(Instant::now(), self.shared.duration_ms) as u64
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms
    }

    /// `true` once playback reached the end (playback then stops; [`play`](Self::play) restarts).
    pub fn ended(&self) -> bool {
        let mut st = self.shared.lock();
        st.tick(Instant::now(), self.shared.duration_ms);
        st.ended
    }

    /// The frame for the current position (`None` until the first frame is decoded).
    /// `serial` changes only when a different frame is returned.
    pub fn frame(&self) -> Option<Frame> {
        let mut st = self.shared.lock();
        let pos = st.tick(Instant::now(), self.shared.duration_ms);
        let epoch = st.epoch;
        // After a seek the old frame is stale: take the first new frame even if it's early.
        let need_new = st.current.as_ref().is_none_or(|(_, g)| *g != epoch);
        let mut min_pts =
            if need_new { i64::MIN } else { st.current.as_ref().map_or(i64::MIN, |(f, _)| f.pts_ms as i64) };
        let mut picked: Option<QFrame> = None;
        while let Some(f) = st.queue.front() {
            if f.pts_ms as f64 <= pos || (need_new && picked.is_none()) {
                let f = st.queue.pop_front().unwrap();
                // Never go backwards (a decoder emitting slightly out of order).
                if f.pts_ms >= min_pts {
                    min_pts = f.pts_ms;
                    if let Some(prev) = picked.replace(f) {
                        st.recycle(prev.rgba);
                    }
                } else {
                    st.recycle(f.rgba);
                }
            } else {
                break;
            }
        }
        if let Some(f) = picked {
            st.serial += 1;
            let (w, h) = self.frame_size;
            let frame = Frame { width: w, height: h, rgba: f.rgba, serial: st.serial, pts_ms: f.pts_ms.max(0) as u64 };
            if let Some((prev, _)) = st.current.replace((frame, epoch)) {
                st.recycle(prev.rgba);
            }
            self.shared.cv.notify_all();
        }
        st.current.as_ref().map(|(f, _)| f.clone())
    }

    /// Peak level (0..1, after volume) of the audio output over the last ~10-50 ms.
    pub fn audio_level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Whether audio is actually being output (false with no audio track or no device).
    pub fn has_audio_output(&self) -> bool {
        self.shared.lock().audio.active
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.cv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Video thread

fn video_thread(sh: &Shared, movie: &Movie, path: &Path, out_w: u32, out_h: u32) {
    let v = &movie.video;
    let track = &v.track;
    let mut dec = match VideoDecoder::new(path, v) {
        Ok(d) => d,
        Err(_) => {
            sh.lock().video_eof = true;
            return;
        }
    };
    // Last presented frame start: seeking past it still shows the last frame.
    let last_pts = (0..track.samples.len()).map(|i| track.pts_ms(i)).max().unwrap_or(0);
    let mut my_epoch = u64::MAX;
    let mut skip_until = 0i64;
    // Presentation time of the last frame queued in this epoch.
    let mut last_queued = i64::MIN;
    let mut spare: Vec<u8> = Vec::new();
    loop {
        let mut st = sh.lock();
        loop {
            if st.shutdown {
                return;
            }
            if st.epoch != my_epoch {
                break;
            }
            // Drop frames the clock has already passed so decoding continues even if the UI
            // isn't polling (keeps the newest due frame for `frame()`).
            let pos = st.pos(Instant::now(), sh.duration_ms);
            while st.queue.len() >= 2 && st.queue[1].pts_ms as f64 <= pos {
                let f = st.queue.pop_front().unwrap();
                st.recycle(f.rgba);
            }
            if !st.video_eof && st.queue.len() < VIDEO_QUEUE {
                break;
            }
            st = sh.cv.wait_timeout(st, TICK).unwrap_or_else(|e| e.into_inner()).0;
        }
        if st.epoch != my_epoch {
            my_epoch = st.epoch;
            skip_until = (st.seek_ms as i64).min(last_pts);
            last_queued = i64::MIN;
            drop(st);
            if dec.restart_at(v.keyframe_before(skip_until)).is_err() {
                sh.lock().video_eof = true;
            }
            continue;
        }
        let pos = st.pos(Instant::now(), sh.duration_ms);
        let playing = st.playing;
        let have_frame = !st.queue.is_empty() || st.current.as_ref().is_some_and(|(_, g)| *g == my_epoch);
        if spare.capacity() == 0
            && let Some(b) = st.pool.pop()
        {
            spare = b;
        }
        drop(st);

        // Frames that will be thrown away: before the seek target, or already late.
        let late = if playing && have_frame { pos as i64 } else { i64::MIN };
        let step = dec.step(v, skip_until.max(late));
        let mut finished = false;
        match step {
            Ok(Step::Picture(p)) => {
                let i = (p.ts as usize).min(track.samples.len() - 1);
                let pts = track.pts_ms(i);
                let end = track.to_ms(track.pts(i) + track.samples[i].dur as i64).max(pts + 1);
                if end <= skip_until {
                    continue; // before the seek target (or pre-roll)
                }
                if playing && have_frame && (end as f64) < pos && pos - (last_queued as f64) < MAX_STALE_MS {
                    continue; // already late: don't spend time converting it
                }
                last_queued = pts;
                convert::to_rgba(&p, out_w, out_h, v.rotation, v.sps.matrix, v.sps.full_range, &mut spare);
                let rgba = std::mem::take(&mut spare);
                let mut st = sh.lock();
                if st.epoch == my_epoch {
                    let at = st.queue.iter().position(|f| f.pts_ms > pts).unwrap_or(st.queue.len());
                    st.queue.insert(at, QFrame { pts_ms: pts, rgba: Arc::new(rgba) });
                }
            }
            Ok(Step::Nothing) => finished = dec.hopeless(),
            Ok(Step::End) | Err(_) => finished = true,
        }
        if finished {
            let mut st = sh.lock();
            if st.epoch == my_epoch {
                st.video_eof = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Audio thread

/// Keeps the output stream alive; dropped when the audio thread exits.
enum Output {
    Cpal(cpal::Stream),
    Null(Option<JoinHandle<()>>),
}

impl Output {
    fn set_playing(&self, on: bool) {
        use cpal::traits::StreamTrait;
        if let Output::Cpal(s) = self {
            let _ = if on { s.play() } else { s.pause() };
        }
    }
}

fn audio_thread(sh: &Arc<Shared>, movie: &Movie, path: &Path, out: AudioOut) {
    let Some(track) = movie.audio.as_ref() else { return };
    let Ok(mut dec) = AacDecoder::new(track) else { return };
    let Ok(mut reader) = SampleReader::open(path) else { return };
    let opened = match out {
        AudioOut::Device => open_device(sh),
        AudioOut::Null => Ok(open_null(sh)),
        AudioOut::Stuck => Ok((Output::Null(None), 48000, 2)),
    };
    let (output, rate, channels) = match opened {
        Ok(o) => o,
        Err(_) => return, // no device: play silently on the wall clock
    };
    {
        let mut st = sh.lock();
        st.audio.active = true;
        st.audio.rate = rate;
        st.audio.channels = channels;
        st.audio.driving = st.playing;
    }
    let cap = rate as usize * channels * AUDIO_BUFFER_MS / 1000;
    let t = &track.track;
    let mut resampler = Resampler::new(track.sample_rate, rate, channels);
    let mut my_epoch = u64::MAX;
    let mut next = 0usize;
    let mut trim_ms = 0f64;
    let mut stream_on = false;
    let mut mixed = Vec::new();
    let mut resampled = Vec::new();

    loop {
        let st = sh.lock();
        if st.shutdown {
            break;
        }
        if st.playing != stream_on {
            stream_on = st.playing;
            output.set_playing(stream_on);
        }
        if st.epoch != my_epoch {
            my_epoch = st.epoch;
            trim_ms = st.seek_ms as f64;
            let target = t.ms_to_ts(st.seek_ms as i64);
            // Start one packet early: AAC needs the previous frame for overlap-add.
            next = (0..t.samples.len()).rev().find(|&i| t.pts(i) <= target).unwrap_or(0).saturating_sub(1);
            dec.reset();
            resampler.reset();
        }
        if st.audio.done || st.audio.ring.len() >= cap {
            let _ = sh.cv.wait_timeout(st, TICK);
            continue;
        }
        drop(st);

        if next >= t.samples.len() {
            let mut st = sh.lock();
            if st.epoch == my_epoch {
                st.audio.done = true;
            }
            continue;
        }
        let s = t.samples[next];
        let pkt_ms = t.pts(next) as f64 * 1000.0 / t.timescale as f64;
        next += 1;
        let Ok(data) = reader.read(&s) else {
            let mut st = sh.lock();
            if st.epoch == my_epoch {
                st.audio.done = true;
            }
            continue;
        };
        let (pcm, in_ch, in_rate) = dec.decode(data);
        let in_rate = in_rate.max(1);
        let frames = pcm.len() / in_ch;
        // Drop decoded samples before the seek target (also handles encoder priming).
        let skip = (((trim_ms - pkt_ms) * in_rate as f64 / 1000.0).ceil().max(0.0) as usize).min(frames);
        if skip == frames {
            continue;
        }
        let first_ms = pkt_ms + skip as f64 * 1000.0 / in_rate as f64;
        mixed.clear();
        remix(&pcm[skip * in_ch..], in_ch, channels, &mut mixed);
        resampled.clear();
        resampler.set_rate(in_rate);
        resampler.process(&mixed, &mut resampled);

        let mut st = sh.lock();
        if st.epoch != my_epoch {
            continue;
        }
        if !st.audio.ring_started {
            st.audio.ring_started = true;
            st.audio.ring_ms = first_ms;
        }
        st.audio.ring.extend(resampled.iter().copied());
    }
    sh.lock().audio.active = false;
    drop(output);
}

/// Fills `data` (interleaved, `ch` channels) from the ring and stamps the audio clock.
fn fill(sh: &Shared, data: &mut [f32], ch: usize, latency: Duration) {
    let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
    let mut st = sh.lock();
    let st = &mut *st;
    let mut written = 0;
    let mut peak = 0f32;
    if st.playing && st.audio.rate > 0 {
        let frames = (data.len() / ch).min(st.audio.ring.len() / ch);
        if frames > 0 {
            let start_ms = st.audio.ring_ms;
            for (d, s) in data[..frames * ch].iter_mut().zip(st.audio.ring.drain(..frames * ch)) {
                let x = (s * vol).clamp(-1.0, 1.0);
                peak = peak.max(x.abs());
                *d = x;
            }
            written = frames * ch;
            st.audio.ring_ms += frames as f64 * 1000.0 / st.audio.rate as f64;
            // What is audible right now was written `latency` ago.
            st.base_ms = start_ms - latency.as_secs_f64() * 1000.0;
            st.at = Instant::now();
            st.audio.driving = true;
        } else if st.audio.done {
            // Audio is over (or shorter than the video): continue on the wall clock.
            st.audio.driving = false;
        }
    }
    data[written..].iter_mut().for_each(|d| *d = 0.0);
    sh.level.store(peak.to_bits(), Ordering::Relaxed);
}

type Opened = (Output, u32, usize);

fn open_device(sh: &Arc<Shared>) -> Result<Opened, String> {
    use cpal::SampleFormat as F;
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no audio output device")?;
    let sup = dev.default_output_config().map_err(|e| e.to_string())?;
    let cfg = sup.config();
    let stream = match sup.sample_format() {
        F::F32 => build::<f32>(&dev, &cfg, sh),
        F::I16 => build::<i16>(&dev, &cfg, sh),
        F::U16 => build::<u16>(&dev, &cfg, sh),
        F::I32 => build::<i32>(&dev, &cfg, sh),
        F::U32 => build::<u32>(&dev, &cfg, sh),
        F::I8 => build::<i8>(&dev, &cfg, sh),
        F::U8 => build::<u8>(&dev, &cfg, sh),
        F::F64 => build::<f64>(&dev, &cfg, sh),
        f => Err(format!("unsupported output sample format {f}")),
    }?;
    let out = Output::Cpal(stream);
    out.set_playing(false);
    Ok((out, cfg.sample_rate, cfg.channels as usize))
}

fn build<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, sh: &Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let ch = cfg.channels.max(1) as usize;
    let sh_data = sh.clone();
    let sh_err = sh.clone();
    let mut tmp: Vec<f32> = Vec::new();
    dev.build_output_stream::<T, _, _>(
        *cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback);
            if tmp.len() < data.len() {
                tmp.resize(data.len(), 0.0); // first callback(s) only
            }
            let buf = &mut tmp[..data.len()];
            fill(&sh_data, buf, ch, latency);
            for (d, s) in data.iter_mut().zip(buf.iter()) {
                *d = T::from_sample(*s);
            }
        },
        move |_err| {
            // Device lost: fall back to the wall clock so video keeps playing.
            let mut st = sh_err.lock();
            st.audio.active = false;
            st.audio.driving = false;
        },
        None,
    )
    .map_err(|e| e.to_string())
}

/// A fake device consuming audio in real time (10 ms periods, 48 kHz stereo).
fn open_null(sh: &Arc<Shared>) -> Opened {
    let sh2 = sh.clone();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0f32; 480 * 2];
        let start = Instant::now();
        let mut periods = 0u32;
        loop {
            if sh2.lock().shutdown {
                break;
            }
            fill(&sh2, &mut buf, 2, Duration::from_millis(20));
            periods += 1;
            let due = start + Duration::from_millis(10) * periods;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    (Output::Null(Some(handle)), 48000, 2)
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Null(h) = self
            && let Some(h) = h.take()
        {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests;
