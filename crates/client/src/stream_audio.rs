//! The sound of screen shares, in stereo at music quality.
//!
//! ```text
//! sharer: stream::AudioCapture (48 kHz stereo, 10 ms) -> Opus (stereo, 192 kbps, 20 ms)
//!         -> [seq u16][opus] sent as ScreenAudio video packets (only watchers get them)
//! watcher: live::deliver -> Listener: jitter buffer -> Opus -> its own speaker stream
//!          (with its own volume), and a mono copy to the echo canceller
//! ```

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use celerity_proto::VideoKind;
use celerity_stream as stream;

use crate::voice::echo::FarEnd;
use crate::voice::jitter::{JitterBuffer, Pop};

/// 20 ms of 48 kHz stereo.
const FRAME: usize = 960;
const BITRATE: i32 = 192_000;

/// Your screen's sound going out while you share.
pub struct AudioShare {
    capture: stream::AudioCapture,
}

impl AudioShare {
    pub fn start(source: stream::AudioSource) -> Result<AudioShare, String> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(|e| format!("couldn't start the audio encoder: {e}"))?;
        let _ = encoder.set_bitrate(opus::Bitrate::Bits(BITRATE));
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(3);
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);
        let mut out = vec![0u8; 4000];
        let mut seq: u16 = 0;
        let capture = stream::AudioCapture::start(source, move |chunk| {
            pending.extend_from_slice(chunk);
            while pending.len() >= FRAME * 2 {
                let frame: Vec<f32> = pending.drain(..FRAME * 2).collect();
                // Nothing playing: send nothing (watchers fill the gap with silence).
                if frame.iter().all(|s| s.abs() < 1e-5) {
                    continue;
                }
                let Ok(n) = encoder.encode_float(&frame, &mut out) else { continue };
                seq = seq.wrapping_add(1);
                let mut packet = Vec::with_capacity(n + 2);
                packet.extend_from_slice(&seq.to_be_bytes());
                packet.extend_from_slice(&out[..n]);
                crate::live::send(VideoKind::ScreenAudio, &packet);
            }
        })?;
        Ok(AudioShare { capture })
    }

    pub fn error(&self) -> Option<String> {
        self.capture.error()
    }
}

/// Someone's screen share sound, playing while you watch it.
pub struct Listener {
    shared: Arc<ListenerShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct ListenerShared {
    jitter: Mutex<JitterBuffer>,
    /// Volume as f32 bits (1.0 = as sent).
    volume: AtomicU32,
    stop: AtomicBool,
}

impl Listener {
    pub fn start(output_device: Option<String>, volume: f32, far_end: Option<Arc<FarEnd>>) -> Listener {
        let shared = Arc::new(ListenerShared {
            jitter: Mutex::new(JitterBuffer::default()),
            volume: AtomicU32::new(volume.to_bits()),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        // Audio streams can't move between threads on every system: open and keep it on one.
        let thread = std::thread::Builder::new()
            .name("celerity-stream-audio".into())
            .spawn(move || {
                let Ok(mut decoder) = opus::Decoder::new(48_000, opus::Channels::Stereo) else { return };
                let mut decoded = vec![0f32; FRAME * 2 * 6];
                let mut pending: Vec<f32> = Vec::new();
                let mut pos = 0;
                let feed = s.clone();
                let source = Box::new(move |out: &mut [f32]| {
                    let mut written = 0;
                    while written < out.len() {
                        if pos >= pending.len() {
                            pending.clear();
                            pos = 0;
                            let pop = feed.jitter.lock().unwrap().pop();
                            let n = match pop {
                                Pop::Packet(p) => decoder.decode_float(&p, &mut decoded, false).unwrap_or(0),
                                Pop::Lost => decoder.decode_float(&[], &mut decoded, false).unwrap_or(0),
                                Pop::Idle => 0,
                            };
                            if n == 0 {
                                pending.resize(FRAME * 2, 0.0);
                            } else {
                                let gain = f32::from_bits(feed.volume.load(Ordering::Relaxed));
                                pending.extend(decoded[..n * 2].iter().map(|x| (x * gain).clamp(-1.0, 1.0)));
                            }
                            if let Some(far) = &far_end {
                                let mono: Vec<f32> =
                                    pending.as_chunks::<2>().0.iter().map(|[l, r]| (l + r) * 0.5).collect();
                                far.push(&mono);
                            }
                        }
                        let n = (pending.len() - pos).min(out.len() - written);
                        out[written..written + n].copy_from_slice(&pending[pos..pos + n]);
                        written += n;
                        pos += n;
                    }
                });
                let stream = match crate::voice::devices::start_output_stereo(output_device.as_deref(), source) {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("couldn't play the stream's sound: {e}");
                        return;
                    }
                };
                while !s.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                drop(stream);
            })
            .ok();
        Listener { shared, thread }
    }

    /// A packet from the network: `[seq u16][opus]`.
    pub fn push(&self, payload: &[u8]) {
        if payload.len() < 3 {
            return;
        }
        let seq = u16::from_be_bytes([payload[0], payload[1]]);
        self.shared.jitter.lock().unwrap().insert(seq, payload[2..].to_vec());
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Which sound to share along with a screen or window, if any can be.
pub fn source_for(screen_id: &str) -> Option<stream::AudioSource> {
    let support = stream::audio_support();
    if screen_id.starts_with("window:") && support.per_app {
        return Some(stream::AudioSource::App { screen_id: screen_id.to_string() });
    }
    support.system.then_some(stream::AudioSource::System)
}

/// What the "share audio" switch says it will share.
pub fn describe(screen_is_window: bool) -> Option<&'static str> {
    let support = stream::audio_support();
    match (screen_is_window && support.per_app, support.system, support.includes_own_audio) {
        (true, _, _) => Some("Also share this app's sound"),
        (false, true, false) => Some("Also share your computer's sound (not this call)"),
        (false, true, true) => {
            Some("Also share your computer's sound (this includes the call, so others may hear an echo)")
        }
        (false, false, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What goes out decodes back to the same sound (Opus is lossy, so: close enough).
    #[test]
    fn round_trip_through_opus() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE)).unwrap();
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut out = vec![0u8; 4000];
        let mut decoded = vec![0f32; FRAME * 2];
        let mut energy = 0.0;
        for f in 0..20 {
            let frame: Vec<f32> = (0..FRAME * 2)
                .map(|i| {
                    let t = (f * FRAME + i / 2) as f32 / 48_000.0;
                    0.4 * (t * 440.0 * std::f32::consts::TAU).sin()
                })
                .collect();
            let n = encoder.encode_float(&frame, &mut out).unwrap();
            let got = decoder.decode_float(&out[..n], &mut decoded, false).unwrap();
            assert_eq!(got, FRAME);
            if f > 5 {
                energy += decoded.iter().map(|x| x * x).sum::<f32>();
            }
        }
        // A 0.4 sine has mean power 0.08: 14 frames x 1920 samples.
        let expected = 0.08 * 14.0 * (FRAME * 2) as f32;
        assert!((energy / expected - 1.0).abs() < 0.2, "{energy} vs {expected}");
    }
}
