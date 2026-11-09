//! Echo cancellation: what comes out of your speakers is taken back out of your
//! microphone, so people don't hear themselves when you're not wearing headphones.
//!
//! The mixer hands every frame it plays to [`FarEnd`]; the capture thread feeds that to
//! WebRTC's echo canceller (AEC3, which finds the delay between the two by itself) just
//! before processing the matching stretch of microphone audio.

use std::collections::VecDeque;
use std::sync::Mutex;

use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig};

/// The canceller works on 10 ms frames.
const CHUNK: usize = 480;
/// At most this much played audio waits to be matched (half a second).
const MAX_QUEUED: usize = 48_000 / 2;

/// What's being played, waiting for the capture thread.
#[derive(Default)]
pub struct FarEnd {
    queue: Mutex<VecDeque<f32>>,
}

impl FarEnd {
    pub fn push(&self, played: &[f32]) {
        let mut q = self.queue.lock().unwrap();
        q.extend(played.iter().copied());
        let extra = q.len().saturating_sub(MAX_QUEUED);
        q.drain(..extra);
    }

    fn take(&self, out: &mut Vec<f32>) {
        let mut q = self.queue.lock().unwrap();
        let n = q.len() / CHUNK * CHUNK;
        out.extend(q.drain(..n));
    }
}

pub struct EchoCanceller3 {
    apm: AudioProcessing,
    render: Vec<f32>,
    out: Vec<f32>,
}

impl EchoCanceller3 {
    pub fn new() -> Self {
        let config = Config { echo_canceller: Some(EchoCanceller::default()), ..Default::default() };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(48_000, 1))
            .render_config(StreamConfig::new(48_000, 1))
            .build();
        EchoCanceller3 { apm, render: Vec::new(), out: vec![0.0; CHUNK] }
    }

    /// Remove the echo of what `far` played from `mic` (48 kHz mono, a multiple of 10 ms).
    pub fn process(&mut self, far: &FarEnd, mic: &mut [f32]) {
        self.render.clear();
        far.take(&mut self.render);
        for chunk in self.render.as_chunks::<CHUNK>().0 {
            let _ = self.apm.process_render_f32(&[chunk], &mut [&mut self.out]);
        }
        for chunk in mic.as_chunks_mut::<CHUNK>().0 {
            if self.apm.process_capture_f32(&[chunk], &mut [&mut self.out]).is_ok() {
                chunk.copy_from_slice(&self.out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Speech-like noise played through a "room" (delayed, quieter) comes back much quieter.
    #[test]
    fn removes_played_audio_from_the_mic() {
        let far = FarEnd::default();
        let mut aec = EchoCanceller3::new();
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let delay = 48 * 40; // 40 ms between the speaker and the mic
        let mut history: VecDeque<f32> = std::iter::repeat_n(0.0, delay).collect();
        let (mut before, mut after) = (0.0, 0.0);
        for frame in 0..400 {
            let played: Vec<f32> = (0..960).map(|_| noise() * 0.5).collect();
            far.push(&played);
            let mut mic: Vec<f32> = played
                .iter()
                .map(|s| {
                    history.push_back(*s);
                    history.pop_front().unwrap() * 0.3
                })
                .collect();
            let raw = level(&mic);
            aec.process(&far, &mut mic);
            if frame >= 300 {
                before += raw;
                after += level(&mic);
            }
        }
        assert!(after < before * 0.25, "echo only went from {before} to {after}");
    }
}
