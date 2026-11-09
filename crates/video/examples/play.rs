//! Headless playback check / benchmark:
//! `cargo run --release -p celerity-video --features playback --example play -- clip.mp4 [max_side]`
//!
//! Plays the file to the end polling `frame()` at ~60 Hz like a UI would, then prints how many
//! distinct frames were shown, the worst frame lag behind the clock and (on Linux) peak RSS.

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn rss_kb(key: &str) -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with(key))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// User + system CPU time of this process in milliseconds (Linux, assumes 100 Hz ticks).
fn cpu_ms() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?) * 10)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: play <file> [max_side]"));
    let max_side: u32 = args.next().map_or(640, |s| s.parse().expect("max_side"));

    println!("RSS before open: {:?} KB", rss_kb("VmRSS:"));
    let t = Instant::now();
    let info = celerity_video::probe(&path).unwrap_or_else(|e| panic!("probe: {e}"));
    println!("probe: {info:?} in {:?}", t.elapsed());
    let t = Instant::now();
    let (_, img) = celerity_video::poster(&path, max_side).unwrap();
    println!("poster: {}x{} in {:?}", img.width(), img.height(), t.elapsed());
    drop(img);
    println!("RSS after poster: {:?} KB (peak {:?} KB)", rss_kb("VmRSS:"), rss_kb("VmHWM:"));

    let player = celerity_video::Player::open(&path, max_side).unwrap();
    while player.frame().is_none() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    let cpu0 = cpu_ms();
    player.play();
    let (mut shown, mut last_serial, mut worst_lag, mut lag_sum, mut samples) = (0u64, 0u64, 0i64, 0i64, 0i64);
    while !player.ended() {
        if let Some(f) = player.frame() {
            if f.serial != last_serial {
                last_serial = f.serial;
                shown += 1;
            }
            let lag = player.position_ms() as i64 - f.pts_ms as i64;
            worst_lag = worst_lag.max(lag);
            lag_sum += lag;
            samples += 1;
        }
        std::thread::sleep(Duration::from_millis(16));
        if start.elapsed() > Duration::from_millis(info.duration_ms * 3 + 5000) {
            println!("timeout!");
            break;
        }
    }
    println!(
        "played {} ms in {:?}: {} frames shown, audio output: {}, mean lag {} ms, worst lag {} ms (includes frame duration)",
        info.duration_ms,
        start.elapsed(),
        shown,
        player.has_audio_output(),
        lag_sum / samples.max(1),
        worst_lag
    );
    if let (Some(a), Some(b)) = (cpu0, cpu_ms()) {
        println!(
            "CPU time during playback: {} ms ({:.0}% of one core)",
            b - a,
            (b - a) as f64 * 100.0 / start.elapsed().as_millis() as f64
        );
    }
    println!("RSS at end: {:?} KB, peak RSS: {:?} KB", rss_kb("VmRSS:"), rss_kb("VmHWM:"));
    let t = Instant::now();
    drop(player);
    println!("drop: {:?}, RSS after drop: {:?} KB", t.elapsed(), rss_kb("VmRSS:"));
}
