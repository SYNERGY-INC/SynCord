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
    /// You muted or unmuted your microphone.
    Mute,
    Unmute,
    /// You deafened or undeafened.
    Deafen,
    Undeafen,
    /// You started sharing your screen or camera.
    Live,
}

/// How a note sounds: overtones (ratio to the note, loudness) and how fast it dies away.
struct Timbre {
    partials: &'static [(f32, f32)],
    /// Seconds for the sound to fall to about a third.
    decay: f32,
}

/// Warm and rounded, like a soft mallet on wood (messages, toggles).
const MARIMBA: Timbre = Timbre { partials: &[(1.0, 1.0), (3.98, 0.18), (9.2, 0.04)], decay: 0.11 };
/// Brighter and ringing, like a small bell (people coming and going, going live).
const BELL: Timbre = Timbre { partials: &[(1.0, 1.0), (2.0, 0.3), (2.76, 0.18), (5.4, 0.06)], decay: 0.18 };

impl Sound {
    /// Notes as (frequency in Hz, start in seconds, length in seconds).
    fn notes(self) -> &'static [(f32, f32, f32)] {
        match self {
            // A quick upward pair (E5 then B5).
            Sound::Message => &[(659.25, 0.0, 0.16), (987.77, 0.07, 0.26)],
            // Rising major arpeggio (C5, E5, G5, C6).
            Sound::Join => &[(523.25, 0.0, 0.14), (659.25, 0.06, 0.14), (783.99, 0.12, 0.14), (1046.5, 0.18, 0.28)],
            // The same, falling.
            Sound::Leave => &[(1046.5, 0.0, 0.14), (783.99, 0.06, 0.14), (659.25, 0.12, 0.14), (523.25, 0.18, 0.28)],
            // A short step down / up.
            Sound::Mute => &[(698.46, 0.0, 0.1), (523.25, 0.05, 0.18)],
            Sound::Unmute => &[(523.25, 0.0, 0.1), (698.46, 0.05, 0.18)],
            // Like mute, an octave lower.
            Sound::Deafen => &[(349.23, 0.0, 0.12), (261.63, 0.06, 0.22)],
            Sound::Undeafen => &[(261.63, 0.0, 0.12), (349.23, 0.06, 0.22)],
            // A sparkle (G5, D6, G6).
            Sound::Live => &[(783.99, 0.0, 0.12), (1174.66, 0.05, 0.12), (1567.98, 0.1, 0.3)],
        }
    }

    fn timbre(self) -> &'static Timbre {
        match self {
            Sound::Join | Sound::Leave | Sound::Live => &BELL,
            _ => &MARIMBA,
        }
    }

    fn slot(self) -> &'static AtomicU64 {
        static MESSAGE: AtomicU64 = AtomicU64::new(0);
        static VOICE: AtomicU64 = AtomicU64::new(0);
        static TOGGLE: AtomicU64 = AtomicU64::new(0);
        match self {
            Sound::Message => &MESSAGE,
            Sound::Join | Sound::Leave | Sound::Live => &VOICE,
            Sound::Mute | Sound::Unmute | Sound::Deafen | Sound::Undeafen => &TOGGLE,
        }
    }

    /// Toggling twice quickly should sound twice; chimes for bursts of messages shouldn't.
    fn min_gap_ms(self) -> u64 {
        match self {
            Sound::Mute | Sound::Unmute | Sound::Deafen | Sound::Undeafen => 80,
            _ => MIN_GAP_MS,
        }
    }
}

/// The sound as 48 kHz mono samples at the given volume (0.0 to 1.0).
pub fn render(sound: Sound, volume: f32) -> Vec<f32> {
    let rate = SAMPLE_RATE as f32;
    let notes = sound.notes();
    let timbre = sound.timbre();
    let end = notes.iter().map(|(_, start, len)| start + len).fold(0.0, f32::max);
    let mut out = vec![0.0f32; (end * rate) as usize + 1];
    let norm: f32 = timbre.partials.iter().map(|(_, a)| a).sum();
    let peak = 0.22 * volume.clamp(0.0, 1.0) / norm;
    for &(freq, start, len) in notes {
        let first = (start * rate) as usize;
        let count = (len * rate) as usize;
        for i in 0..count {
            let t = i as f32 / rate;
            // A soft attack (no click), an exponential ring, and a fade to silence at the end.
            let attack = (t / 0.004).min(1.0);
            let ring = (-t / timbre.decay).exp();
            let tail = (1.0 - t / len).clamp(0.0, 1.0).sqrt();
            let mut tone = 0.0;
            for &(ratio, amp) in timbre.partials {
                // Higher overtones fade faster, like a real struck object.
                let fade = (-t * ratio * 4.0).exp();
                tone += amp * fade * (std::f32::consts::TAU * freq * ratio * t).sin();
            }
            if let Some(s) = out.get_mut(first + i) {
                *s += tone * attack * ring * tail * peak;
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
    if now.saturating_sub(last) < sound.min_gap_ms() || volume <= 0.0 {
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
        use Sound::*;
        for sound in [Message, Join, Leave, Mute, Unmute, Deafen, Undeafen, Live] {
            let s = render(sound, 1.0);
            let secs = s.len() as f32 / SAMPLE_RATE as f32;
            assert!(secs > 0.2 && secs < 0.5, "{sound:?} lasts {secs}s");
            assert!(s.iter().all(|v| v.abs() <= 1.0), "{sound:?} clips");
            assert!(s.iter().any(|v| v.abs() > 0.03), "{sound:?} is silent");
            // Starts and ends quietly (no click).
            assert!(s[0].abs() < 0.01 && s[s.len() - 1].abs() < 0.01);
        }
        assert!(render(Sound::Message, 0.0).iter().all(|v| *v == 0.0));
    }
}
