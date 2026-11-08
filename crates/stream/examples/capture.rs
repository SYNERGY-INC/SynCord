//! Lists devices, or captures from one and reports frame rate and CPU use.
//!
//! ```text
//! cargo run --release -p syncord-stream --example capture                 # list
//! cargo run --release -p syncord-stream --example capture -- screen [ID] [SECONDS] [FPS] [MAXW] [MAXH]
//! cargo run --release -p syncord-stream --example capture -- camera [ID] [SECONDS] [FPS] [W] [H]
//! cargo run --release -p syncord-stream --example capture -- send-screen [ID] [SECONDS] [FPS] [MAXW] [MAXH]
//! ```
//! `send-screen` runs the whole sender (capture + encode + packetize) and a receiver.
//! The last captured frame is written to `capture-last.ppm`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use syncord_stream::*;

fn cpu_time() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
}

fn rss_mb() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<f64>().ok()? / 1024.0)
}

/// Writes `f` as a binary PPM (BT.709 limited range -> RGB), for eyeballing the capture.
fn save_ppm(f: &I420Frame) {
    let (w, h) = (f.width() as usize, f.height() as usize);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            let yy = (f.y()[y * w + x] as f32 - 16.0) * 1.164;
            let u = f.u()[(y / 2) * (w / 2) + x / 2] as f32 - 128.0;
            let v = f.v()[(y / 2) * (w / 2) + x / 2] as f32 - 128.0;
            let c = |x: f32| x.clamp(0.0, 255.0) as u8;
            out.extend_from_slice(&[c(yy + 1.793 * v), c(yy - 0.213 * u - 0.533 * v), c(yy + 2.112 * u)]);
        }
    }
    let _ = std::fs::write("capture-last.ppm", out);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("list");
    let num = |i: usize, d: u32| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    match mode {
        "list" => {
            match list_cameras() {
                Ok(c) if c.is_empty() => println!("cameras: none"),
                Ok(c) => c.iter().for_each(|d| println!("camera  {:?}  id={}", d.name, d.id)),
                Err(e) => println!("cameras: error: {e}"),
            }
            match list_screens() {
                Ok(s) => s.iter().for_each(|d| {
                    println!(
                        "screen  {:?} {}x{} {:?}{}  id={}",
                        d.name,
                        d.width,
                        d.height,
                        d.kind,
                        if d.primary { " primary" } else { "" },
                        d.id
                    )
                }),
                Err(e) => println!("screens: error: {e}"),
            }
        }
        "screen" | "camera" | "send-screen" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let id = if id.is_empty() || id == "-" {
                if mode == "camera" {
                    list_cameras().ok().and_then(|c| c.first().map(|d| d.id.clone())).unwrap_or_default()
                } else {
                    list_screens()
                        .ok()
                        .and_then(|s| s.iter().find(|d| d.primary).map(|d| d.id.clone()))
                        .unwrap_or_default()
                }
            } else {
                id
            };
            let secs = num(2, 5);
            let fps = num(3, if mode == "camera" { 30 } else { 15 });
            let (mw, mh) =
                (num(4, if mode == "camera" { 640 } else { 1920 }), num(5, if mode == "camera" { 360 } else { 1080 }));
            let frames = Arc::new(Mutex::new((0u64, I420Frame::default())));
            let f2 = frames.clone();
            let c0 = cpu_time();
            let t0 = Instant::now();
            if mode == "send-screen" {
                let rx = Arc::new(Receiver::new(ReceiverConfig::new(1)));
                let rx2 = rx.clone();
                let mut cfg = SenderConfig::screen(1);
                cfg.encoder.max_fps = fps as f32;
                cfg.max_width = mw;
                cfg.max_height = mh;
                let sender = Sender::start(
                    cfg,
                    Source::Screen {
                        id,
                        config: ScreenConfig { max_width: mw, max_height: mh, fps, show_cursor: true },
                    },
                    move |d| rx2.push_packet(d),
                )
                .unwrap_or_else(|e| panic!("{e}"));
                std::thread::sleep(Duration::from_secs(secs as u64));
                let cpu = c0.zip(cpu_time()).map(|(a, b)| (b - a) / t0.elapsed().as_secs_f64() * 100.0);
                let s = sender.stats();
                println!("sender: {s:?}");
                println!("receiver: {:?}", rx.stats());
                println!(
                    "{:.1} fps encoded, {:.0} kbps, process CPU {:.1}% of one core (capture+encode+decode), RSS {:.1} MB",
                    s.frames_encoded as f64 / secs as f64,
                    s.bytes as f64 * 8.0 / secs as f64 / 1000.0,
                    cpu.unwrap_or(f64::NAN),
                    rss_mb().unwrap_or(f64::NAN)
                );
                if let Some(e) = sender.error() {
                    println!("error: {e}");
                }
                return;
            }
            let sink = move |f: &I420Frame| {
                let mut g = f2.lock().unwrap();
                g.0 += 1;
                g.1.copy_from(f);
            };
            let cap = if mode == "camera" {
                Capture::camera(&id, CameraConfig { width: mw, height: mh, fps }, sink)
            } else {
                Capture::screen(&id, ScreenConfig { max_width: mw, max_height: mh, fps, show_cursor: true }, sink)
            };
            let cap = match cap {
                Ok(c) => c,
                Err(e) => {
                    println!("could not start: {e}");
                    std::process::exit(1);
                }
            };
            std::thread::sleep(Duration::from_secs(secs as u64));
            let wall = t0.elapsed().as_secs_f64();
            let cpu = c0.zip(cpu_time()).map(|(a, b)| (b - a) / wall * 100.0);
            let g = frames.lock().unwrap();
            println!(
                "{} frames in {wall:.1} s = {:.1} fps at {}x{}, process CPU {:.1}% of one core, RSS {:.1} MB",
                g.0,
                g.0 as f64 / wall,
                g.1.width(),
                g.1.height(),
                cpu.unwrap_or(f64::NAN),
                rss_mb().unwrap_or(f64::NAN)
            );
            if let Some(e) = cap.error() {
                println!("error: {e}");
            }
            save_ppm(&g.1);
        }
        _ => println!("usage: capture [list | screen | camera | send-screen] [ID] [SECONDS] [FPS] [W] [H]"),
    }
}
