//! Short notification sounds, generated in code so there are no audio files to ship.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use celerity_proto::voice::SAMPLE_RATE;

/// Don't play the same kind of sound more often than this (a burst of messages makes one chime).
const MIN_GAP_MS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// A new message (direct or in a server channel).
    Message,
    /// Someone joined your voice channel.
    Join,
    /// Someone left your voice channel.
    Leave,
}

impl Sound {
    /// Notes as (frequency in Hz, start in seconds, length in seconds).
    fn notes(self) -> &'static [(f32, f32, f32)] {
        match self {
            // Two quick bright notes (A5 then E6).
            Sound::Message => &[(880.0, 0.0, 0.12), (1318.5, 0.09, 0.22)],
            // Rising (C5, E5, G5).
            Sound::Join => &[(523.25, 0.0, 0.12), (659.25, 0.08, 0.12), (783.99, 0.16, 0.22)],
            // Falling (G5, E5, C5).
            Sound::Leave => &[(783.99, 0.0, 0.12), (659.25, 0.08, 0.12), (523.25, 0.16, 0.22)],
        }
    }

    fn slot(self) -> &'static AtomicU64 {
        static MESSAGE: AtomicU64 = AtomicU64::new(0);
        static VOICE: AtomicU64 = AtomicU64::new(0);
        match self {
            Sound::Message => &MESSAGE,
            Sound::Join | Sound::Leave => &VOICE,
        }
    }
}

/// The sound as 48 kHz mono samples at the given volume (0.0 to 1.0).
pub fn render(sound: Sound, volume: f32) -> Vec<f32> {
    let rate = SAMPLE_RATE as f32;
    let notes = sound.notes();
    let end = notes.iter().map(|(_, start, len)| start + len).fold(0.0, f32::max);
    let mut out = vec![0.0f32; (end * rate) as usize + 1];
    let peak = 0.25 * volume.clamp(0.0, 1.0);
    for &(freq, start, len) in notes {
        let first = (start * rate) as usize;
        let count = (len * rate) as usize;
        for i in 0..count {
            let t = i as f32 / rate;
            // Quick fade in, then a smooth decay, so there are no clicks.
            let attack = (t / 0.005).min(1.0);
            let decay = (1.0 - t / len).max(0.0).powi(2);
            // A little of the octave above makes it sound like a soft bell rather than a beep.
            let tone = (std::f32::consts::TAU * freq * t).sin() + 0.25 * (std::f32::consts::TAU * freq * 2.0 * t).sin();
            if let Some(s) = out.get_mut(first + i) {
                *s += tone * attack * decay * peak;
            }
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Play a sound in the background on the given output device (`None` is the system default).
/// Does nothing if the same kind of sound played very recently.
pub fn play(sound: Sound, device: Option<String>, volume: f32) {
    let now = now_ms();
    let last = sound.slot().load(Ordering::Relaxed);
    if now.saturating_sub(last) < MIN_GAP_MS || volume <= 0.0 {
        return;
    }
    sound.slot().store(now, Ordering::Relaxed);

    let samples = render(sound, volume);
    let length = Duration::from_secs_f32(samples.len() as f32 / SAMPLE_RATE as f32);
    let _ = std::thread::Builder::new().name("sound".into()).spawn(move || {
        let mut pos = 0;
        let source = Box::new(move |buf: &mut [f32]| {
            for s in buf.iter_mut() {
                *s = samples.get(pos).copied().unwrap_or(0.0);
                pos += 1;
            }
        });
        match crate::voice::devices::start_output_source(device.as_deref(), source) {
            // Keep the stream open until the sound has finished, plus a little for the device's buffer.
            Ok(_stream) => std::thread::sleep(length + Duration::from_millis(250)),
            Err(e) => eprintln!("couldn't play sound: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_and_never_clip() {
        for sound in [Sound::Message, Sound::Join, Sound::Leave] {
            let s = render(sound, 1.0);
            let secs = s.len() as f32 / SAMPLE_RATE as f32;
            assert!(secs > 0.2 && secs < 0.5, "{sound:?} lasts {secs}s");
            assert!(s.iter().all(|v| v.abs() <= 1.0), "{sound:?} clips");
            assert!(s.iter().any(|v| v.abs() > 0.05), "{sound:?} is silent");
            // Starts and ends quietly (no click).
            assert!(s[0].abs() < 0.01 && s[s.len() - 1].abs() < 0.01);
        }
        assert!(render(Sound::Message, 0.0).iter().all(|v| *v == 0.0));
    }
}
