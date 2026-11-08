//! Headless player tests (no sound card needed: the device path silently falls back to the wall
//! clock, and `AudioOut::Null` simulates a real-time audio device).

use std::time::{Duration, Instant};

use super::{AudioOut, Player};
use crate::tests::{SRC, counter_clip, fixture, high_aac, rotated_clip};

fn wait_until(timeout_ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

fn no_audio_clip() -> Option<std::path::PathBuf> {
    fixture("player_noaudio.mp4", &format!("{SRC} -t 2 -c:v libx264 -profile:v high -pix_fmt yuv420p"))
}

#[test]
fn plays_without_audio_on_wall_clock() {
    let Some(p) = no_audio_clip() else { return };
    let player = Player::open(&p, 160).unwrap();
    assert_eq!(player.duration_ms(), 2000);
    assert!(!player.is_playing());
    // First frame is available while still paused at 0.
    assert!(wait_until(3000, || player.frame().is_some()), "no first frame");
    let f = player.frame().unwrap();
    assert_eq!((f.width, f.height), (160, 120));
    assert_eq!(f.rgba.len(), 160 * 120 * 4);
    assert_eq!(player.position_ms(), 0);

    player.play();
    assert!(player.is_playing());
    let start = Instant::now();
    let mut serials = vec![f.serial];
    while start.elapsed() < Duration::from_millis(700) {
        let f = player.frame().unwrap();
        if f.serial != *serials.last().unwrap() {
            serials.push(f.serial);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(serials.windows(2).all(|w| w[1] > w[0]));
    assert!(serials.len() >= 10, "only {} distinct frames in 700 ms", serials.len());
    let pos = player.position_ms();
    assert!((600..=900).contains(&pos), "position {pos}");

    // Seek forward while playing; position and frame follow.
    player.seek(1500);
    let pos = player.position_ms();
    assert!((1500..=1600).contains(&pos), "position after seek {pos}");
    assert!(wait_until(2000, || player.frame().is_some_and(|f| f.pts_ms >= 1466)), "frame after seek");

    assert!(wait_until(3000, || player.ended()), "never ended");
    assert!(!player.is_playing());
    assert_eq!(player.position_ms(), 2000);

    // play() after the end restarts from the beginning.
    player.play();
    assert!(player.position_ms() < 200);
    assert!(wait_until(2000, || player.frame().is_some_and(|f| f.pts_ms < 300)));
    assert!(!player.ended());
}

#[test]
fn pause_freezes_position() {
    let Some(p) = no_audio_clip() else { return };
    let player = Player::open(&p, 64).unwrap();
    player.play();
    std::thread::sleep(Duration::from_millis(300));
    player.pause();
    let a = player.position_ms();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(player.position_ms(), a);
    assert!(!player.is_playing());
    let s = player.frame().map(|f| f.serial);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(player.frame().map(|f| f.serial), s, "frame changed while paused");
}

/// Seeking (while paused) shows exactly the frame at the target, decoding forward from the
/// previous keyframe through B-frames.
#[test]
fn seek_shows_exact_frame() {
    let Some(p) = counter_clip() else { return };
    let player = Player::open(&p, 64).unwrap();
    // Frame n has luma 3n (limited range) and lasts 40 ms.
    let gray = |n: i32| ((3 * n - 16) as f32 * 255.0 / 219.0).round() as i32;
    for (target, n) in [(1000u64, 25), (1039, 25), (1040, 26), (0, 0), (2350, 58), (520, 13), (5000, 59)] {
        player.seek(target);
        assert!(
            wait_until(3000, || player.frame().is_some_and(|f| f.pts_ms == (n as u64) * 40)),
            "seek {target}: got {:?}",
            player.frame().map(|f| f.pts_ms)
        );
        let f = player.frame().unwrap();
        let px = f.rgba[(32 * 64 + 32) * 4] as i32;
        assert!((px - gray(n).max(0)).abs() <= 3, "seek {target}: pixel {px}, want {}", gray(n));
    }
}

#[test]
fn audio_drives_clock() {
    let Some(p) = high_aac() else { return };
    let player = Player::open_with(&p, 320, AudioOut::Null).unwrap();
    assert!(player.info().has_audio);
    assert!(wait_until(2000, || player.has_audio_output()));
    assert!(wait_until(2000, || player.frame().is_some()));
    player.play();
    std::thread::sleep(Duration::from_millis(1000));
    let pos = player.position_ms();
    // Null device reports 20 ms latency; allow scheduling slack.
    assert!((850..=1150).contains(&pos), "position {pos}");
    let lvl = player.audio_level();
    // ffmpeg's sine source has amplitude 1/8.
    assert!((0.08..=0.2).contains(&lvl), "level {lvl}");
    player.set_volume(0.0);
    assert!(wait_until(500, || player.audio_level() == 0.0));
    player.set_volume(2.0);
    assert!(wait_until(500, || player.audio_level() > 0.2));
    // Video keeps up with the audio clock.
    let f = player.frame().unwrap();
    let pos = player.position_ms();
    assert!(f.pts_ms <= pos + 40 && f.pts_ms + 150 >= pos, "frame {} vs clock {pos}", f.pts_ms);

    player.seek(2500);
    assert!(wait_until(3000, || player.ended()), "never ended");
    assert_eq!(player.position_ms(), player.duration_ms());
}

/// An audio device that opens but never consumes samples must not freeze playback.
#[test]
fn stuck_audio_device_falls_back_to_wall_clock() {
    let Some(p) = high_aac() else { return };
    let player = Player::open_with(&p, 64, AudioOut::Stuck).unwrap();
    assert!(wait_until(2000, || player.has_audio_output()));
    player.play();
    std::thread::sleep(Duration::from_millis(500));
    // Waiting for audio: the clock stops at the extrapolation cap.
    assert!(player.position_ms() <= 300, "{}", player.position_ms());
    assert!(wait_until(2000, || player.position_ms() > 1000), "clock stuck at {}", player.position_ms());
    assert!(wait_until(4000, || player.ended()), "never ended");
}

#[test]
fn device_fallback_and_drop_is_prompt() {
    // On machines without a sound card the device can't be opened; playback still works.
    let Some(p) = high_aac() else { return };
    let player = Player::open(&p, 100).unwrap();
    player.play();
    assert!(wait_until(1500, || player.position_ms() > 200), "clock not running");
    let t = Instant::now();
    drop(player);
    assert!(t.elapsed() < Duration::from_millis(500), "drop took {:?}", t.elapsed());
}

#[test]
fn rotated_frames() {
    let Some(p) = rotated_clip(90) else { return };
    let player = Player::open(&p, 64).unwrap();
    assert!(wait_until(3000, || player.frame().is_some()));
    let f = player.frame().unwrap();
    assert_eq!((f.width, f.height), (32, 64));
    // The white top-left quadrant is now top-right.
    let lum = |x: usize, y: usize| f.rgba[(y * 32 + x) * 4];
    assert!(lum(24, 16) > 200 && lum(8, 48) < 50);
}

#[test]
fn player_is_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Player>();
}
