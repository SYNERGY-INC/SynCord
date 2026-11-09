//! End-to-end tests without devices: synthetic frames -> encode -> packetize -> lossy network ->
//! reassemble -> decode.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn pattern(n: u64, w: u32, h: u32) -> I420Frame {
    let mut f = I420Frame::new(w, h);
    render_test_pattern(n, &mut f);
    f
}

#[test]
fn encode_decode_roundtrip_quality() {
    for (kind, w, h, bps) in [(ContentKind::Camera, 640, 360, 800_000), (ContentKind::Screen, 1280, 720, 2_500_000)] {
        let mut enc =
            Encoder::new(EncoderConfig { content: kind, bitrate_bps: bps, ..EncoderConfig::camera() }).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = I420Frame::default();
        let mut worst: f64 = 0.0;
        let mut keyframes = 0;
        for n in 0..30u64 {
            let src = pattern(n, w, h);
            let Some(ef) = enc.encode(&src, n * 33).unwrap() else { continue };
            keyframes += ef.keyframe as u32;
            if n == 0 {
                assert!(ef.keyframe, "first frame must be a keyframe");
                // SPS (type 7) leads the keyframe.
                assert_eq!(&ef.data[..5], &[0, 0, 0, 1, 0x67]);
            }
            let data = ef.data.to_vec();
            let pic = dec.decode(&data).unwrap().expect("picture");
            assert_eq!((pic.width(), pic.height()), (w, h));
            pic.to_i420(&mut out);
            worst = worst.max(luma_difference(&src, &out));
        }
        assert_eq!(keyframes, 1, "{kind:?}: only the first frame should be a keyframe");
        assert!(worst < 8.0, "{kind:?}: mean luma error {worst}");
    }
}

#[test]
fn keyframe_interval_force_and_bitrate_change() {
    let mut enc =
        Encoder::new(EncoderConfig { keyframe_interval: Duration::from_secs(1), ..EncoderConfig::camera() }).unwrap();
    let mut keys = Vec::new();
    for n in 0..70u64 {
        if n == 40 {
            enc.force_keyframe();
            enc.set_bitrate(300_000);
            enc.set_max_fps(15.0);
        }
        if let Some(ef) = enc.encode(&pattern(n, 320, 180), n * 33).unwrap()
            && ef.keyframe
        {
            keys.push(n);
        }
    }
    // Frame 0, ~1 s later (frame 31), the forced one at 40, ~1 s after that.
    assert_eq!(keys, [0, 31, 40, 71 - 1].iter().copied().filter(|&k| k < 70).collect::<Vec<_>>());
}

#[test]
fn size_change_restarts_with_keyframe() {
    let mut enc = Encoder::new(EncoderConfig::camera()).unwrap();
    let mut dec = Decoder::new().unwrap();
    for (n, (w, h)) in [(320, 180), (320, 180), (640, 360), (640, 360)].into_iter().enumerate() {
        let ef = enc.encode(&pattern(n as u64, w, h), n as u64 * 33).unwrap().unwrap();
        assert_eq!(ef.keyframe, n % 2 == 0);
        let pic = dec.decode(ef.data).unwrap().unwrap();
        assert_eq!(pic.width(), w);
    }
}

/// The full chain under loss, duplication and reordering: the picture must match the source
/// whenever a frame is shown, and recover after the receiver asks for a keyframe.
#[test]
fn lossy_network_recovers_after_forced_keyframe() {
    let (w, h) = (640, 360);
    let mut enc = Encoder::new(EncoderConfig { bitrate_bps: 1_000_000, ..EncoderConfig::camera() }).unwrap();
    let mut asm = Reassembler::new(7, ReassemblerConfig::default());
    let mut dec = Decoder::new().unwrap();
    let mut rng = Rng(42);
    let t0 = Instant::now();
    let mut sources = std::collections::HashMap::new();
    let mut in_flight: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut frame_no = 0u32;
    let mut shown = Vec::new();
    let mut requests = Vec::new();
    let mut decoded = I420Frame::default();
    let mut lost_frame = None;
    for n in 0..150u64 {
        let now_ms = n * 33;
        let src = pattern(n, w, h);
        if let Some(ef) = enc.encode(&src, now_ms).unwrap() {
            sources.insert(frame_no, src);
            let mut pk = Vec::new();
            packetize(7, frame_no, ef.keyframe, now_ms as u32, ef.data, DEFAULT_MAX_PACKET, |d| pk.push(d.to_vec()))
                .unwrap();
            for (i, p) in pk.into_iter().enumerate() {
                // Lose one fragment of frame 40, and 1% of all packets after frame 100.
                if (frame_no == 40 && i == 0) || (frame_no > 100 && rng.below(100) == 0) {
                    lost_frame.get_or_insert(frame_no);
                    continue;
                }
                let at = now_ms + rng.below(40) as u64; // jitter reorders across frames
                if rng.below(20) == 0 {
                    in_flight.push((at + 5, p.clone())); // duplicate
                }
                in_flight.push((at, p));
            }
            frame_no += 1;
        }
        // Deliver everything due by now, in arrival order.
        in_flight.sort_by_key(|p| p.0);
        let due = in_flight.iter().take_while(|p| p.0 <= now_ms).count();
        for (at, p) in in_flight.drain(..due).collect::<Vec<_>>() {
            let now = t0 + Duration::from_millis(at);
            let _ = asm.push(&p, now);
            while let Some(cf) = asm.pop(now) {
                let pic = dec.decode(&cf.data).unwrap_or_else(|e| panic!("frame {} undecodable: {e:?}", cf.frame));
                let pic = pic.expect("picture");
                pic.to_i420(&mut decoded);
                let diff = luma_difference(&sources[&cf.frame], &decoded);
                assert!(diff < 8.0, "frame {}: mean luma error {diff}", cf.frame);
                shown.push((cf.frame, cf.keyframe));
            }
        }
        // The receiver asks for a keyframe (rate limited to one per 500 ms, as Receiver does).
        if asm.needs_keyframe() && requests.last().is_none_or(|&r| now_ms >= r + 500) {
            requests.push(now_ms);
            enc.force_keyframe();
        }
    }
    let lost = 40;
    assert_eq!(lost_frame, Some(lost));
    let frames: Vec<u32> = shown.iter().map(|s| s.0).collect();
    assert!(frames.windows(2).all(|w| w[0] < w[1]));
    assert!(frames.contains(&(lost - 1)) && !frames.contains(&lost));
    // Recovery: a keyframe after the loss, shown within ~0.5 s of the loss.
    let recovered = shown.iter().find(|s| s.0 > lost && s.1).expect("no keyframe after the loss").0;
    assert!(recovered - lost < 16, "recovered only at frame {recovered}");
    assert!(!requests.is_empty());
    // Most frames make it despite the later random loss.
    assert!(shown.len() > 100, "only {} of {frame_no} frames shown", shown.len());
    eprintln!(
        "lossy e2e: {} of {frame_no} frames shown, keyframe requests at {:?} ms, recovered at frame {recovered}",
        shown.len(),
        requests
    );
}

/// Sender and Receiver threads wired through a lossy "network".
#[test]
fn sender_receiver_pipelines() {
    let wire: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let w2 = wire.clone();
    let mut cfg = SenderConfig::camera(5);
    cfg.max_width = 320;
    cfg.max_height = 180;
    cfg.preview_max_side = Some(160);
    let sender = Sender::start(cfg, Source::Manual, move |d| w2.lock().unwrap().push(d.to_vec())).unwrap();
    let rx = Receiver::new(ReceiverConfig { max_side: 200, ..ReceiverConfig::new(5) });
    let mut last_serial = 0;
    let mut drop_from = None;
    let mut asked = 0;
    let t0 = now_us();
    for n in 0..90u64 {
        // A 640x360 source gets scaled to 320x180 by the sender.
        let mut f = pattern(n, 640, 360);
        f.timestamp_us = t0 + n * 33_333;
        sender.push_frame(&f);
        std::thread::sleep(Duration::from_millis(12));
        let packets: Vec<Vec<u8>> = std::mem::take(&mut *wire.lock().unwrap());
        for p in packets {
            // Drop everything for frames 30..33: the receiver must ask for a keyframe.
            let (h, _) = PacketHeader::parse(&p).unwrap();
            if (30..33).contains(&h.frame) {
                drop_from.get_or_insert(n);
                continue;
            }
            rx.push_packet(&p);
        }
        if rx.take_keyframe_request() {
            asked += 1;
            sender.force_keyframe();
        }
        if let Some(fr) = rx.frame() {
            assert!(fr.serial >= last_serial);
            last_serial = fr.serial;
            assert_eq!((fr.width, fr.height), (200, 112));
            assert_eq!(fr.rgba.len(), 200 * 112 * 4);
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    let ss = sender.stats();
    let rs = rx.stats();
    eprintln!("sender {ss:?}\nreceiver {rs:?}");
    assert_eq!((ss.width, ss.height), (320, 180));
    // Loose bounds: debug builds on a loaded machine encode slowly and the mailbox drops frames.
    assert!(ss.frames_encoded > 30, "{ss:?}");
    assert!(ss.keyframes >= 2, "forced keyframe missing: {ss:?}");
    assert!(asked >= 1);
    assert!(rs.frames_decoded > 20, "{rs:?}");
    assert!(!rx.needs_keyframe());
    let p = sender.preview().expect("preview");
    assert_eq!((p.width, p.height), (160, 90));
    assert!(sender.error().is_none());
}

#[test]
fn test_pattern_source_runs() {
    let count = Arc::new(Mutex::new(0usize));
    let c2 = count.clone();
    let sender = Sender::start(
        SenderConfig { max_width: 320, max_height: 240, ..SenderConfig::camera(1) },
        Source::TestPattern { width: 320, height: 240, fps: 30 },
        move |_| *c2.lock().unwrap() += 1,
    )
    .unwrap();
    // Poll (debug builds on a busy machine are slow to get going).
    let t = Instant::now();
    while *count.lock().unwrap() <= 5 && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(sender);
    assert!(*count.lock().unwrap() > 5);
}

fn glyph(f: &mut I420Frame, row: usize, col: usize, rng: &mut Rng) {
    let wd = f.width() as usize;
    let (y, _, _) = f.planes_mut();
    for gy in 0..12 {
        for gx in 0..7 {
            if rng.below(3) == 0 {
                y[(row * 16 + gy + 20) * wd + col * 9 + gx + 20] = 16;
            }
        }
    }
}

/// Text-like content (dark glyphs on white, ten new characters "typed" per frame) through both
/// encoder modes: `cargo test --release -p celerity-stream text_quality -- --ignored --nocapture`
#[test]
#[ignore]
fn text_quality() {
    let (w, h) = (1920u32, 1080u32);
    let mut rng = Rng(7);
    let mut page = I420Frame::new(w, h);
    page.planes_mut().0.fill(235);
    for row in 0..40 {
        for col in 0..180 {
            glyph(&mut page, row, col, &mut rng);
        }
    }
    for (name, usage) in [("screen-content mode", true), ("camera mode", false)] {
        for bps in [1_500_000u32, 2_500_000] {
            let cfg = EncoderConfig {
                bitrate_bps: bps,
                max_fps: 15.0,
                screen_content_tools: usage,
                ..EncoderConfig::screen()
            };
            let mut enc = Encoder::new(cfg).unwrap();
            let mut dec = Decoder::new().unwrap();
            let mut f = page.clone();
            let mut out = I420Frame::default();
            let (mut err, mut n, mut bytes, mut ms) = (0.0, 0, 0usize, 0.0);
            let mut rng2 = Rng(99);
            for i in 0..60u64 {
                for k in 0..10 {
                    let c = i as usize * 10 + k;
                    glyph(&mut f, 41 + c / 180, c % 180, &mut rng2);
                }
                let t = Instant::now();
                let Some(ef) = enc.encode(&f, i * 66).unwrap() else { continue };
                ms += t.elapsed().as_secs_f64() * 1000.0;
                bytes += ef.data.len();
                if let Ok(Some(p)) = dec.decode(ef.data) {
                    p.to_i420(&mut out);
                    if i >= 30 {
                        err += luma_difference(&f, &out);
                        n += 1;
                    }
                }
            }
            eprintln!(
                "{name} @{} kbps: mean luma error {:.2} (frames 30..60), sent {:.0} kbps, encode {:.1} ms/frame",
                bps / 1000,
                err / n.max(1) as f64,
                bytes as f64 * 8.0 / 4.0 / 1000.0,
                ms / 60.0
            );
        }
    }
}

/// Receivers can hand out I420 planes (for GPU conversion) and switch at run time.
#[test]
fn receiver_i420_output() {
    let wire: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let w2 = wire.clone();
    let cfg = SenderConfig { max_width: 640, max_height: 360, hardware_encoder: false, ..SenderConfig::camera(3) };
    let sender = Sender::start(cfg, Source::Manual, move |d| w2.lock().unwrap().push(d.to_vec())).unwrap();
    let rx = Receiver::new(ReceiverConfig { output: Output::I420, max_side: 640, ..ReceiverConfig::new(3) });
    let t0 = now_us();
    let mut seen_i420 = None;
    let mut seen_rgba = None;
    let feed = |n: u64| {
        let mut f = pattern(n, 640, 360);
        f.timestamp_us = t0 + n * 33_333;
        sender.push_frame(&f);
        std::thread::sleep(Duration::from_millis(15));
        for p in std::mem::take(&mut *wire.lock().unwrap()) {
            rx.push_packet(&p);
        }
    };
    // Slow (debug, loaded) machines: run until both kinds were seen, switching after the first.
    for n in 0..200u64 {
        if seen_i420.is_some() && seen_rgba.is_some() {
            break;
        }
        feed(n);
        if seen_i420.is_some() {
            rx.set_output(Output::Rgba);
        }
        let Some(fr) = rx.frame() else { continue };
        match fr.pixels() {
            Pixels::I420 { y, u, v, y_stride, uv_stride } => {
                assert_eq!((fr.width, fr.height, y_stride, uv_stride), (640, 360, 640, 320));
                assert_eq!((y.len(), u.len(), v.len()), (640 * 360, 320 * 180, 320 * 180));
                assert!(fr.rgba.is_empty());
                // The picture matches one of the frames sent.
                let f = fr.i420.as_ref().unwrap();
                let best = (0..=n).map(|k| luma_difference(&pattern(k, 640, 360), f)).fold(f64::MAX, f64::min);
                assert!(best < 8.0, "I420 output far from every source frame ({best})");
                seen_i420.get_or_insert(fr.serial);
            }
            Pixels::Rgba(px) => {
                assert_eq!(px.len(), 640 * 360 * 4);
                assert!(fr.i420.is_none());
                seen_rgba.get_or_insert(fr.serial);
            }
        }
    }
    assert!(seen_i420.is_some() && seen_rgba.is_some(), "{seen_i420:?} {seen_rgba:?}");
    // Scaled I420 output keeps the aspect ratio.
    rx.set_output(Output::I420);
    rx.set_max_side(320);
    for n in 200..220u64 {
        feed(n);
    }
    let fr = rx.frame().unwrap();
    assert_eq!((fr.width, fr.height), (320, 180));
    assert_eq!(fr.i420.as_ref().map(|f| (f.width(), f.height())), Some((320, 180)));
    assert_eq!(sender.stats().encoder, "openh264");
}

#[test]
fn quality_presets() {
    for q in ScreenQuality::ALL {
        let c = SenderConfig::screen_quality(9, q);
        assert_eq!((c.max_width, c.max_height), q.max_size());
        assert_eq!(c.encoder.max_fps, q.fps() as f32);
        assert_eq!(c.encoder.bitrate_bps, q.bitrate_bps());
        assert!(c.fit_bitrate && c.max_packet == DEFAULT_MAX_PACKET);
        #[cfg(feature = "capture")]
        assert_eq!(q.screen_config(true).fps, q.fps());
    }
    assert_eq!(ScreenQuality::default(), ScreenQuality::Res1440p60);
    assert_eq!(ScreenQuality::Res1440p60.bitrate_bps(), 12_000_000);
    assert_eq!(ScreenQuality::Source60.max_size(), (3840, 2160));
    for q in CameraQuality::ALL {
        let c = SenderConfig::camera_quality(1, q);
        assert_eq!((c.max_width, c.max_height), q.size());
        #[cfg(feature = "capture")]
        assert_eq!(q.camera_config().width, q.size().0);
    }
    assert_eq!(CameraQuality::Res1080p.bitrate_bps(), 3_500_000);
}

/// A 4K preset sharing a 720p source sends 720p's bitrate; `set_bitrate` stays the cap.
#[test]
fn fit_bitrate_follows_the_source_size() {
    let mut cfg = SenderConfig::screen_quality(4, ScreenQuality::Source60);
    cfg.hardware_encoder = false;
    let sender = Sender::start(cfg, Source::Manual, |_| {}).unwrap();
    let t0 = now_us();
    let push = |n: u64| {
        let mut f = pattern(n, 1280, 720);
        f.timestamp_us = t0 + n * 16_667;
        sender.push_frame(&f);
        let t = Instant::now();
        while sender.stats().frames_encoded + sender.stats().frames_skipped <= n && t.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    push(0);
    let fitted = sender.stats().bitrate_bps;
    assert_eq!(fitted, recommended_bitrate(ContentKind::Screen, 1280, 720, 60.0));
    assert!(fitted < 6_000_000, "{fitted}");
    sender.set_bitrate(1_000_000);
    push(1);
    assert_eq!(sender.stats().bitrate_bps, 1_000_000);
}

/// Each frame's packets leave spread over a few milliseconds, not in one burst, and in order.
#[test]
fn packets_are_paced() {
    type Arrivals = Vec<(Instant, u32, u16)>;
    let arrivals: Arc<Mutex<Arrivals>> = Arc::default();
    let a2 = arrivals.clone();
    let mut cfg = SenderConfig::screen_quality(6, ScreenQuality::Res720p30);
    cfg.hardware_encoder = false;
    let sender = Sender::start(cfg, Source::Manual, move |d| {
        let (h, _) = PacketHeader::parse(d).unwrap();
        a2.lock().unwrap().push((Instant::now(), h.frame, h.index));
    })
    .unwrap();
    // A noisy picture makes a big keyframe (hundreds of packets).
    let mut f = I420Frame::new(1280, 720);
    let mut rng = Rng(5);
    f.planes_mut().0.iter_mut().for_each(|p| *p = 16 + rng.below(220) as u8);
    f.timestamp_us = now_us();
    sender.push_frame(&f);
    let t = Instant::now();
    while sender.stats().frames_encoded == 0 && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    let a = arrivals.lock().unwrap();
    assert!(a.len() > 50, "only {} packets", a.len());
    assert!(a.windows(2).all(|w| (w[0].1, w[0].2) < (w[1].1, w[1].2)), "out of order");
    let spread = a.last().unwrap().0 - a[0].0;
    eprintln!("{} packets spread over {spread:?}", a.len());
    assert!(spread >= Duration::from_millis(4), "burst: {spread:?}");
    assert!(spread <= Duration::from_millis(60), "too slow: {spread:?}");
}

/// Encoder speed with the presets:
/// `cargo test --release -p celerity-stream preset_speed -- --ignored --nocapture`
#[test]
#[ignore]
fn preset_speed() {
    for q in [ScreenQuality::Res1080p30, ScreenQuality::Res1080p60, ScreenQuality::Res1440p60] {
        let (w, h) = q.max_size();
        let cfg = SenderConfig::screen_quality(1, q);
        let mut enc = Encoder::new(cfg.encoder.clone()).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut f = I420Frame::new(w, h);
        let (mut ms, mut dms, mut bytes, mut n) = (0.0, 0.0, 0usize, 0);
        for i in 0..60u64 {
            render_test_pattern(i, &mut f);
            let t = Instant::now();
            let Some(ef) = enc.encode(&f, i * 1000 / q.fps() as u64).unwrap() else { continue };
            ms += t.elapsed().as_secs_f64() * 1000.0;
            bytes += ef.data.len();
            n += 1;
            let data = ef.data.to_vec();
            let t = Instant::now();
            dec.decode(&data).unwrap();
            dms += t.elapsed().as_secs_f64() * 1000.0;
        }
        eprintln!(
            "{}: encode {:.1} ms, decode {:.1} ms per frame, {:.1} Mbps (target {:.1})",
            q.label(),
            ms / n as f64,
            dms / n as f64,
            bytes as f64 * 8.0 / (60.0 / q.fps() as f64) / 1e6,
            q.bitrate_bps() as f64 / 1e6
        );
    }
}
