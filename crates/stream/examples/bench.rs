//! Encode/decode/convert cost and memory on synthetic frames.
//!
//! `cargo run --release -p syncord-stream --example bench`
//!
//! Two kinds of content: "moving" (the whole test pattern changes every frame: worst case) and
//! "static" (a mostly still picture with a small area changing, like a document being typed in).

use std::time::Instant;

use syncord_stream::*;

/// Process CPU time in seconds (Linux: /proc; elsewhere: None).
fn cpu_time() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("VmRSS:"))?.to_string();
            Some(line.split_whitespace().nth(1)?.parse::<f64>().ok()? / 1024.0)
        })
        .unwrap_or(f64::NAN)
}

fn frame(n: u64, w: u32, h: u32, still: bool, out: &mut I420Frame) {
    if still {
        // The picture only changes every 15th frame, plus a small "cursor" block each frame.
        render_test_pattern(n / 15, out);
        let (fw, _) = (out.width() as usize, out.height() as usize);
        let (y, _, _) = out.planes_mut();
        let x0 = 100 + (n as usize % 40) * 8;
        for row in 200..216 {
            y[row * fw + x0..row * fw + x0 + 8].fill(235);
        }
    } else {
        render_test_pattern(n, out);
    }
    let _ = (w, h);
}

fn main() {
    let frames = 150u64;
    println!("{frames} frames each; ms are per frame; CPU% is of one core at the given fps\n");
    for (name, kind, w, h, bps, fps, still) in [
        ("camera 640x360@30 moving", ContentKind::Camera, 640, 360, 700_000, 30.0, false),
        ("camera 1280x720@30 moving", ContentKind::Camera, 1280, 720, 1_500_000, 30.0, false),
        ("screen 1280x720@15 moving", ContentKind::Screen, 1280, 720, 2_000_000, 15.0, false),
        ("screen 1280x720@15 static", ContentKind::Screen, 1280, 720, 2_000_000, 15.0, true),
        ("screen 1920x1080@15 moving", ContentKind::Screen, 1920, 1080, 3_000_000, 15.0, false),
        ("screen 1920x1080@15 static", ContentKind::Screen, 1920, 1080, 3_000_000, 15.0, true),
    ] {
        let rss0 = rss_mb();
        let mut enc =
            Encoder::new(EncoderConfig { content: kind, bitrate_bps: bps, max_fps: fps, ..EncoderConfig::camera() })
                .unwrap();
        let mut f = I420Frame::new(w, h);
        let mut encoded = Vec::new();
        let (mut enc_wall, mut enc_cpu) = (0.0, 0.0);
        for n in 0..frames {
            frame(n, w, h, still, &mut f);
            let (c0, t0) = (cpu_time(), Instant::now());
            let r = enc.encode(&f, (n as f64 * 1000.0 / fps as f64) as u64).unwrap();
            enc_wall += t0.elapsed().as_secs_f64();
            enc_cpu += c0.zip(cpu_time()).map_or(0.0, |(a, b)| b - a);
            if let Some(ef) = r {
                encoded.push(ef.data.to_vec());
            }
        }
        let rss_enc = rss_mb() - rss0;
        let bytes: usize = encoded.iter().map(|e| e.len()).sum();
        let kbps = bytes as f64 * 8.0 / (frames as f64 / fps as f64) / 1000.0;

        let rss1 = rss_mb();
        let mut dec = Decoder::new().unwrap();
        let mut rgba = Vec::new();
        let (c0, t0) = (cpu_time(), Instant::now());
        for e in &encoded {
            if let Ok(Some(p)) = dec.decode(e) {
                p.to_rgba(w.max(h), &mut rgba);
            }
        }
        let dec_wall = t0.elapsed().as_secs_f64();
        let dec_cpu = c0.zip(cpu_time()).map(|(a, b)| b - a);
        let rss_dec = rss_mb() - rss1;
        let per = |t: f64| t * 1000.0 / frames as f64;
        let pct = |t: f64| t / frames as f64 * fps as f64 * 100.0;
        // /proc CPU ticks are 10 ms, so per-frame CPU is only meaningful summed over many frames.
        let ec = if enc_cpu > 0.0 { enc_cpu } else { enc_wall };
        let dc = dec_cpu.unwrap_or(dec_wall);
        println!(
            "{name:27} enc {:5.2} ms wall / {:5.2} ms cpu ({:4.1}%) {:5.0} kbps (target {:4})  dec+rgba {:5.2} ms ({:4.1}%)  mem enc {:4.1} MB dec {:4.1} MB",
            per(enc_wall),
            per(ec),
            pct(ec),
            kbps,
            bps / 1000,
            per(dec_wall),
            pct(dc),
            rss_enc,
            rss_dec
        );
        drop(enc);
        drop(dec);
    }

    // Capture-side conversion: BGRA (DXGI/X11) at screen size -> I420 at the send size.
    println!();
    for (sw, sh, mw, mh) in
        [(1920, 1080, 1920, 1080), (1920, 1080, 1280, 720), (2560, 1440, 1920, 1080), (3840, 2160, 1920, 1080)]
    {
        let bgra: Vec<u8> =
            (0..sw * sh).flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255]).collect();
        let n = 30;
        let t0 = Instant::now();
        let mut out = I420Frame::default();
        for _ in 0..n {
            bgra_to_i420(&bgra, sw as u32, sh as u32, mw, mh, &mut out);
        }
        println!(
            "BGRA {sw}x{sh} -> I420 {}x{}: {:.2} ms per frame",
            out.width(),
            out.height(),
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }
    println!("\nRSS at end: {:.1} MB", rss_mb());
}
