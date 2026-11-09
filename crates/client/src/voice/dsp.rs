//! Audio processing for the microphone path: resampling, level metering,
//! noise suppression, the voice-activity gate and Opus encoding.

use nnnoiseless::DenoiseState;
use celerity_proto::voice::FRAME_SAMPLES;

/// Streaming linear resampler. Linear interpolation is plenty for speech.
pub struct Resampler {
    step: f64,
    t: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self { step: from_rate as f64 / to_rate as f64, t: 0.0, last: 0.0 }
    }

    pub fn is_passthrough(&self) -> bool {
        self.step == 1.0
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        for &x in input {
            while self.t < 1.0 {
                out.push(self.last + (x - self.last) * self.t as f32);
                self.t += self.step;
            }
            self.t -= 1.0;
            self.last = x;
        }
    }
}

/// Loudness of a frame in dBFS (0 = full scale, quieter is more negative).
pub fn level_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let mean_sq = frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32;
    (10.0 * mean_sq.max(1e-10).log10()).max(-100.0)
}

/// Opens when the input is louder than the threshold, and stays open briefly
/// afterwards so the ends of words aren't clipped.
pub struct VoiceGate {
    hold: u32,
}

/// 300 ms of hang time after the voice drops below the threshold.
const HOLD_FRAMES: u32 = 15;

impl VoiceGate {
    pub fn new() -> Self {
        Self { hold: 0 }
    }

    pub fn update(&mut self, level_db: f32, threshold_db: f32) -> bool {
        if level_db >= threshold_db {
            self.hold = HOLD_FRAMES;
            true
        } else if self.hold > 0 {
            self.hold -= 1;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureSettings {
    pub gain: f32,
    pub threshold_db: f32,
    pub noise_suppression: bool,
    pub muted: bool,
}

pub struct CaptureOutput {
    pub level_db: f32,
    /// Encoded Opus frame with its sequence number, if this frame should be sent.
    pub packet: Option<(u16, Vec<u8>)>,
}

/// Turns 20 ms microphone frames into Opus packets.
pub struct CaptureProcessor {
    encoder: opus::Encoder,
    denoise: Box<DenoiseState<'static>>,
    gate: VoiceGate,
    seq: u16,
    scratch: Vec<f32>,
    encoded: Vec<u8>,
}

impl CaptureProcessor {
    pub fn new() -> Result<Self, opus::Error> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(64_000))?;
        // Forward error correction lets the receiver recover from a lost packet.
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        Ok(Self {
            encoder,
            denoise: DenoiseState::new(),
            gate: VoiceGate::new(),
            seq: 0,
            scratch: vec![0.0; DenoiseState::FRAME_SIZE],
            encoded: vec![0; 1275],
        })
    }

    pub fn process(&mut self, frame: &mut [f32], s: CaptureSettings) -> CaptureOutput {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);
        if s.gain != 1.0 {
            frame.iter_mut().for_each(|x| *x *= s.gain);
        }
        if s.noise_suppression {
            // RNNoise works on 10 ms chunks in 16-bit sample range.
            for chunk in frame.chunks_mut(DenoiseState::FRAME_SIZE) {
                for (dst, src) in self.scratch.iter_mut().zip(chunk.iter()) {
                    *dst = src * 32767.0;
                }
                self.denoise.process_frame(chunk, &self.scratch);
                chunk.iter_mut().for_each(|x| *x /= 32767.0);
            }
        }
        frame.iter_mut().for_each(|x| *x = x.clamp(-1.0, 1.0));

        let level = level_db(frame);
        let open = self.gate.update(level, s.threshold_db);
        if s.muted || !open {
            return CaptureOutput { level_db: level, packet: None };
        }
        match self.encoder.encode_float(frame, &mut self.encoded) {
            Ok(len) => {
                self.seq = self.seq.wrapping_add(1);
                CaptureOutput { level_db: level, packet: Some((self.seq, self.encoded[..len].to_vec())) }
            }
            Err(e) => {
                eprintln!("opus encode failed: {e}");
                CaptureOutput { level_db: level, packet: None }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * freq * std::f32::consts::TAU / 48_000.0).sin()).collect()
    }

    #[test]
    fn resampler_lengths() {
        let input = vec![0.5f32; 4410];
        let mut out = Vec::new();
        Resampler::new(44_100, 48_000).process(&input, &mut out);
        assert!((out.len() as i64 - 4800).abs() <= 2, "{}", out.len());

        let mut out = Vec::new();
        Resampler::new(48_000, 16_000).process(&vec![0.0; 960], &mut out);
        assert_eq!(out.len(), 320);
    }

    #[test]
    fn levels() {
        assert_eq!(level_db(&[0.0; 960]), -100.0);
        let full = level_db(&sine(440.0, 1.0, 960));
        assert!((full + 3.0).abs() < 0.2, "a full-scale sine is about -3 dBFS, got {full}");
    }

    #[test]
    fn gate_holds_then_closes() {
        let mut g = VoiceGate::new();
        assert!(g.update(-20.0, -40.0));
        for _ in 0..HOLD_FRAMES {
            assert!(g.update(-80.0, -40.0));
        }
        assert!(!g.update(-80.0, -40.0));
    }

    #[test]
    fn encodes_speech_and_skips_silence() {
        let mut cap = CaptureProcessor::new().unwrap();
        let settings = CaptureSettings { gain: 1.0, threshold_db: -45.0, noise_suppression: false, muted: false };

        let mut quiet = vec![0.0; FRAME_SAMPLES];
        assert!(cap.process(&mut quiet, settings).packet.is_none());

        let mut loud = sine(300.0, 0.3, FRAME_SAMPLES);
        let out = cap.process(&mut loud, settings);
        let (seq, bytes) = out.packet.expect("loud audio is sent");
        assert_eq!(seq, 1);
        assert!(!bytes.is_empty() && bytes.len() < 400);

        let mut loud = sine(300.0, 0.3, FRAME_SAMPLES);
        let muted = CaptureSettings { muted: true, ..settings };
        assert!(cap.process(&mut loud, muted).packet.is_none(), "muted never sends");
    }

    #[test]
    fn noise_suppression_runs() {
        let mut cap = CaptureProcessor::new().unwrap();
        let settings = CaptureSettings { gain: 1.0, threshold_db: -100.0, noise_suppression: true, muted: false };
        for _ in 0..5 {
            let mut f = sine(200.0, 0.2, FRAME_SAMPLES);
            let out = cap.process(&mut f, settings);
            assert!(out.level_db.is_finite());
        }
    }
}
