//! Decodes everyone's audio and mixes it into one stream for the speakers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use celerity_proto::voice::{Direction, FRAME_SAMPLES, Key, Packet, open};

use super::dsp::level_db;
use super::jitter::{JitterBuffer, Pop};

/// Someone counts as "speaking" if we played audio above this level recently.
const SPEAKING_DB: f32 = -50.0;
const SPEAKING_HOLD: Duration = Duration::from_millis(250);
/// Free a speaker's decoder after this long without packets.
const FORGET_AFTER: Duration = Duration::from_secs(60);

struct Stream {
    decoder: opus::Decoder,
    jitter: JitterBuffer,
    last_packet: Instant,
    last_loud: Option<Instant>,
}

struct Inner {
    streams: HashMap<u32, Stream>,
    /// Mixed audio not yet handed to the speakers.
    pending: Vec<f32>,
    pending_pos: usize,
    volume: f32,
    /// Per-speaker volume (by ssrc), 1.0 = normal.
    speaker_volume: HashMap<u32, f32>,
    decoded: Vec<f32>,
    mixed: Vec<f32>,
}

pub struct Mixer {
    inner: Mutex<Inner>,
    /// What's played, for the echo canceller.
    pub far_end: std::sync::Arc<super::echo::FarEnd>,
    /// Audio frames that arrived in time, and ones that were missing (for the connection meter).
    pub heard: std::sync::atomic::AtomicU64,
    pub missed: std::sync::atomic::AtomicU64,
    /// Key for opening encrypted audio from the server.
    key: Mutex<Option<Key>>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                streams: HashMap::new(),
                pending: Vec::new(),
                pending_pos: 0,
                volume: 1.0,
                speaker_volume: HashMap::new(),
                decoded: vec![0.0; FRAME_SAMPLES],
                mixed: vec![0.0; FRAME_SAMPLES],
            }),
            key: Mutex::new(None),
            far_end: Default::default(),
            heard: Default::default(),
            missed: Default::default(),
        }
    }
}

impl Mixer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_key(&self, key: Option<Key>) {
        *self.key.lock().unwrap() = key;
    }

    /// Accept a server -> client audio packet (from UDP or the WebSocket).
    pub fn push_packet(&self, data: &[u8]) {
        let key = *self.key.lock().unwrap();
        match Packet::parse(data) {
            // With encryption on, plain audio is refused: it could be forged.
            Some(Packet::AudioDown { ssrc, seq, opus }) if key.is_none() => self.push(ssrc, seq, opus),
            Some(Packet::SealedDown { ssrc, counter, sealed }) => {
                if let Some((seq, opus)) = key.and_then(|k| open(&k, Direction::Down, counter, sealed))
                    && let Some(opus) = crate::e2ee::open_media(ssrc, &opus)
                {
                    self.push(ssrc, seq, &opus);
                }
            }
            _ => {}
        }
    }

    pub fn push(&self, ssrc: u32, seq: u16, opus: &[u8]) {
        let mut inner = self.lock();
        let stream = match inner.streams.entry(ssrc) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let Ok(decoder) = opus::Decoder::new(48_000, opus::Channels::Mono) else { return };
                v.insert(Stream {
                    decoder,
                    jitter: JitterBuffer::default(),
                    last_packet: Instant::now(),
                    last_loud: None,
                })
            }
        };
        stream.last_packet = Instant::now();
        stream.jitter.insert(seq, opus.to_vec());
    }

    pub fn set_volume(&self, volume: f32) {
        self.lock().volume = volume.clamp(0.0, 2.0);
    }

    pub fn set_speaker_volume(&self, ssrc: u32, volume: f32) {
        self.lock().speaker_volume.insert(ssrc, volume.clamp(0.0, 2.0));
    }

    /// Fill `out` with 48 kHz mono audio, decoding and mixing as needed.
    pub fn fill(&self, out: &mut [f32]) {
        let mut inner = self.lock();
        let mut written = 0;
        while written < out.len() {
            if inner.pending_pos >= inner.pending.len() {
                let (heard, missed) = inner.mix_frame();
                self.heard.fetch_add(heard, std::sync::atomic::Ordering::Relaxed);
                self.missed.fetch_add(missed, std::sync::atomic::Ordering::Relaxed);
                self.far_end.push(&inner.pending);
            }
            let available = &inner.pending[inner.pending_pos..];
            let n = available.len().min(out.len() - written);
            out[written..written + n].copy_from_slice(&available[..n]);
            written += n;
            inner.pending_pos += n;
        }
    }

    /// Speakers (by ssrc) heard in the last moment.
    pub fn speaking(&self) -> Vec<u32> {
        let inner = self.lock();
        inner
            .streams
            .iter()
            .filter(|(_, s)| s.last_loud.is_some_and(|t| t.elapsed() < SPEAKING_HOLD))
            .map(|(ssrc, _)| *ssrc)
            .collect()
    }
}

impl Inner {
    /// Mix the next 20 ms. Returns how many speakers' frames arrived, and how many were missing.
    fn mix_frame(&mut self) -> (u64, u64) {
        let (mut heard, mut missed) = (0, 0);
        self.mixed.iter_mut().for_each(|x| *x = 0.0);
        self.streams.retain(|_, s| s.last_packet.elapsed() < FORGET_AFTER);

        for (ssrc, stream) in self.streams.iter_mut() {
            let n = match stream.jitter.pop() {
                Pop::Idle => continue,
                Pop::Packet(p) => {
                    heard += 1;
                    stream.decoder.decode_float(&p, &mut self.decoded, false)
                }
                // An empty packet asks Opus to conceal the gap smoothly.
                Pop::Lost => {
                    missed += 1;
                    stream.decoder.decode_float(&[], &mut self.decoded, false)
                }
            };
            let Ok(n) = n else { continue };
            let samples = &self.decoded[..n.min(FRAME_SAMPLES)];
            if level_db(samples) > SPEAKING_DB {
                stream.last_loud = Some(Instant::now());
            }
            let gain = self.volume * self.speaker_volume.get(ssrc).copied().unwrap_or(1.0);
            for (m, s) in self.mixed.iter_mut().zip(samples) {
                *m += s * gain;
            }
        }

        for m in self.mixed.iter_mut() {
            *m = m.clamp(-1.0, 1.0);
        }
        std::mem::swap(&mut self.pending, &mut self.mixed);
        self.pending_pos = 0;
        if self.mixed.len() != FRAME_SAMPLES {
            self.mixed.resize(FRAME_SAMPLES, 0.0);
        }
        (heard, missed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::dsp::{CaptureProcessor, CaptureSettings};
    use super::*;

    fn sine_frame(phase: &mut f32, amp: f32) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .map(|_| {
                *phase += 300.0 * std::f32::consts::TAU / 48_000.0;
                amp * phase.sin()
            })
            .collect()
    }

    #[test]
    fn silence_when_nobody_talks() {
        let m = Mixer::default();
        let mut out = vec![1.0; 2000];
        m.fill(&mut out);
        assert!(out.iter().all(|x| *x == 0.0));
        assert!(m.speaking().is_empty());
    }

    #[test]
    fn decodes_and_mixes_two_speakers() {
        let mixer = Mixer::default();
        let settings = CaptureSettings { gain: 1.0, threshold_db: -60.0, noise_suppression: false, muted: false };
        let mut a = CaptureProcessor::new().unwrap();
        let mut b = CaptureProcessor::new().unwrap();
        let (mut pa, mut pb) = (0.0, 1.0);

        for _ in 0..10 {
            let (seq, pkt) = a.process(&mut sine_frame(&mut pa, 0.3), settings).packet.unwrap();
            mixer.push(1, seq, &pkt);
            let (seq, pkt) = b.process(&mut sine_frame(&mut pb, 0.3), settings).packet.unwrap();
            mixer.push(2, seq, &pkt);
        }

        let mut out = vec![0.0; FRAME_SAMPLES * 8];
        mixer.fill(&mut out);
        let level = level_db(&out[FRAME_SAMPLES * 2..]);
        assert!(level > -20.0, "mixed audio should be clearly audible, got {level} dB");
        let mut speaking = mixer.speaking();
        speaking.sort();
        assert_eq!(speaking, vec![1, 2]);

        mixer.set_volume(0.0);
        mixer.fill(&mut out[..FRAME_SAMPLES]);
        assert!(out[..FRAME_SAMPLES].iter().all(|x| *x == 0.0), "volume 0 is silent");
    }

    #[test]
    fn ignores_malformed_packets() {
        let m = Mixer::default();
        m.push_packet(&[1, 2, 3]);
        m.push(5, 1, &[0xFF, 0xFF, 0xFF]); // not valid Opus
        m.push(5, 2, &[0xFF]);
        m.push(5, 3, &[0xFF]);
        let mut out = vec![0.0; FRAME_SAMPLES * 3];
        m.fill(&mut out); // must not panic
    }
}
