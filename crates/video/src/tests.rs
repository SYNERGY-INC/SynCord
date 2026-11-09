//! Tests against clips generated with ffmpeg at test time (nothing binary is committed).
//! Tests that need ffmpeg print a notice and pass when it is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::{poster, probe};

/// Directory for generated fixtures (kept between runs as a cache; ~3 MB).
fn fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join("celerity-video-fixtures-v1");
        std::fs::create_dir_all(&d).unwrap();
        d
    })
}

/// A per-process path for files a test writes itself (and removes).
fn scratch(name: &str) -> PathBuf {
    fixture_dir().join(format!("scratch-{}-{name}", std::process::id()))
}

pub(crate) fn have_ffmpeg() -> bool {
    static HAVE: OnceLock<bool> = OnceLock::new();
    *HAVE.get_or_init(|| {
        let ok = Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("ffmpeg not found: skipping tests that need generated video fixtures");
        }
        ok
    })
}

fn have_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(name))
}

/// Generates `name` with the given ffmpeg arguments (output path appended) unless it exists.
/// Returns `None` (test should skip) if ffmpeg is missing.
pub(crate) fn fixture(name: &str, args: &str) -> Option<PathBuf> {
    if !have_ffmpeg() {
        return None;
    }
    // The cache key includes the arguments, so editing a fixture's recipe regenerates it.
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        args.hash(&mut h);
        h.finish()
    };
    let path = fixture_dir().join(format!("{hash:016x}-{name}"));
    // Tests run in parallel; serialize generation of the same file.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !path.exists() {
        // Unique temp name: other test processes may be generating the same fixture.
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let tmp = fixture_dir().join(format!("tmp-{}-{name}.{ext}", std::process::id()));
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(args.split_whitespace())
            .arg(&tmp)
            .output()
            .expect("run ffmpeg");
        assert!(out.status.success(), "ffmpeg failed for {name}: {}", String::from_utf8_lossy(&out.stderr));
        std::fs::rename(&tmp, &path).unwrap();
    }
    Some(path)
}

pub(crate) const SRC: &str = "-f lavfi -i testsrc2=size=320x240:rate=30";
pub(crate) const TONE: &str = "-f lavfi -i sine=frequency=440:sample_rate=48000";

pub(crate) fn high_aac() -> Option<PathBuf> {
    fixture(
        "high_aac.mp4",
        &format!("{SRC} {TONE} -t 3 -c:v libx264 -profile:v high -pix_fmt yuv420p -bf 3 -g 30 -c:a aac -shortest"),
    )
}

/// Frames whose luma encodes the frame number, to verify presentation order exactly.
pub(crate) fn counter_clip() -> Option<PathBuf> {
    fixture(
        "counter.mp4",
        "-f lavfi -i nullsrc=size=64x64:rate=25,geq=lum='mod(N*3,256)':cb=128:cr=128 -frames:v 60 \
         -c:v libx264 -profile:v high -bf 3 -b-pyramid normal -g 25 -pix_fmt yuv420p -qp 4",
    )
}

#[test]
fn probe_h264_aac() {
    let Some(p) = high_aac() else { return };
    let info = probe(&p).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert!(info.has_audio);
    assert!((2900..=3100).contains(&info.duration_ms), "{info:?}");
}

#[test]
fn poster_h264_aac() {
    let Some(p) = high_aac() else { return };
    let (info, img) = poster(&p, 160).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert_eq!(img.dimensions(), (160, 120));
    // testsrc2 is colorful: make sure we didn't produce a flat image.
    let px: Vec<_> = img.pixels().map(|p| p.0).collect();
    let distinct = px.iter().collect::<std::collections::HashSet<_>>().len();
    assert!(distinct > 50, "only {distinct} distinct colors");
    assert!(px.iter().all(|p| p[3] == 255));
}

#[test]
fn no_audio_baseline() {
    let Some(p) =
        fixture("baseline_noaudio.mp4", &format!("{SRC} -t 2 -c:v libx264 -profile:v baseline -pix_fmt yuv420p"))
    else {
        return;
    };
    let info = probe(&p).unwrap();
    assert!(!info.has_audio);
    assert_eq!((info.width, info.height), (320, 240));
    let (_, img) = poster(&p, 640).unwrap();
    assert_eq!(img.dimensions(), (320, 240));
}

#[test]
fn main_profile() {
    let Some(p) = fixture("main.mp4", &format!("{SRC} -t 1 -c:v libx264 -profile:v main -pix_fmt yuv420p")) else {
        return;
    };
    let (_, img) = poster(&p, 100).unwrap();
    assert_eq!(img.dimensions(), (100, 75));
}

#[test]
fn odd_sizes() {
    // H.264 4:2:0 can only crop in steps of 2, so truly odd sizes can't be encoded; use sizes that
    // are not multiples of 16 (cropped macroblocks) and odd scaled output sizes instead.
    let Some(p) = fixture(
        "odd.mp4",
        "-f lavfi -i testsrc2=size=322x242:rate=24 -t 1 -c:v libx264 -profile:v high -pix_fmt yuv420p",
    ) else {
        return;
    };
    let info = probe(&p).unwrap();
    assert_eq!((info.width, info.height), (322, 242));
    let (_, img) = poster(&p, 641).unwrap();
    assert_eq!(img.dimensions(), (322, 242));
    let (_, img) = poster(&p, 101).unwrap();
    assert_eq!(img.dimensions(), (101, 76));
    // Right / bottom edges must be real picture, not padding garbage: testsrc2 has no black edge.
    let (_, img) = poster(&p, 1000).unwrap();
    let edge = img.get_pixel(321, 120).0;
    assert!(edge[0] as u32 + edge[1] as u32 + edge[2] as u32 > 30, "{edge:?}");

    let Some(p) = fixture(
        "odd2.mp4",
        "-f lavfi -i testsrc2=size=334x198:rate=30 -t 1 -c:v libx264 -profile:v baseline -pix_fmt yuv420p",
    ) else {
        return;
    };
    let (info, img) = poster(&p, 333).unwrap();
    assert_eq!((info.width, info.height), (334, 198));
    assert_eq!(img.dimensions(), (333, 197));
}

#[test]
fn mov_container() {
    let Some(p) =
        fixture("clip.mov", &format!("{SRC} {TONE} -t 2 -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest -f mov"))
    else {
        return;
    };
    let info = probe(&p).unwrap();
    assert!(info.has_audio, "{info:?}");
    assert_eq!((info.width, info.height), (320, 240));
    poster(&p, 64).unwrap();
}

/// A 128x64 clip, white only in the top-left quadrant, with a clockwise display rotation of `cw`
/// degrees in its track header.
pub(crate) fn rotated_clip(cw: i32) -> Option<PathBuf> {
    let base = fixture(
        "rot_base.mp4",
        "-f lavfi -i nullsrc=size=128x64:rate=25,geq=lum='if(lt(X,W/2)*lt(Y,H/2),235,16)':cb=128:cr=128 \
         -frames:v 5 -c:v libx264 -pix_fmt yuv420p",
    )?;
    // ffmpeg's -display_rotation is counter-clockwise.
    fixture(&format!("rot{cw}.mp4"), &format!("-display_rotation {} -i {} -c copy", -cw, base.display()))
}

/// Phone videos are stored landscape with a rotation in the track header matrix; `probe` reports
/// the displayed size and frames come out upright.
#[test]
fn rotation_metadata() {
    if !have_ffmpeg() {
        return;
    }
    // (clockwise rotation, displayed size, where the white quadrant ends up as (right, bottom))
    let cases = [
        (0, (128, 64), (false, false)),
        (90, (64, 128), (true, false)),
        (180, (128, 64), (true, true)),
        (270, (64, 128), (false, true)),
    ];
    for (cw, size, (right, bottom)) in cases {
        let p = rotated_clip(cw).unwrap();
        assert_eq!(crate::demux::open(&p).unwrap().video.rotation, cw as u16, "rotation {cw}");
        let info = probe(&p).unwrap();
        assert_eq!((info.width, info.height), size, "rotation {cw}");
        let (_, img) = poster(&p, 32).unwrap();
        let (w, h) = img.dimensions();
        assert_eq!((w, h), (size.0 / 4, size.1 / 4), "rotation {cw}");
        let lum = |x: u32, y: u32| img.get_pixel(x, y).0[0];
        let (x, y) = (if right { w * 3 / 4 } else { w / 4 }, if bottom { h * 3 / 4 } else { h / 4 });
        assert!(lum(x, y) > 200, "rotation {cw}: white quadrant not at ({x}, {y})");
        assert!(lum(w - 1 - x, h - 1 - y) < 50, "rotation {cw}");
    }
}

#[test]
fn rejects_hevc() {
    if !have_ffmpeg() || !have_encoder("libx265") {
        eprintln!("libx265 not available, skipping");
        return;
    }
    let p = fixture("hevc.mp4", &format!("{SRC} -t 1 -c:v libx265 -x265-params log-level=error -tag:v hvc1")).unwrap();
    let e = probe(&p).unwrap_err();
    assert!(e.contains("HEVC"), "{e}");
    assert!(poster(&p, 100).unwrap_err().contains("HEVC"));
}

#[test]
fn rejects_vp9() {
    if !have_ffmpeg() || !have_encoder("libvpx-vp9") {
        eprintln!("libvpx-vp9 not available, skipping");
        return;
    }
    let p = fixture("vp9.mp4", &format!("{SRC} -t 1 -c:v libvpx-vp9 -deadline realtime -cpu-used 8")).unwrap();
    let e = probe(&p).unwrap_err();
    assert!(e.contains("VP9"), "{e}");
}

#[test]
fn rejects_high444_and_10bit() {
    let Some(p) = fixture("h444.mp4", &format!("{SRC} -t 1 -c:v libx264 -pix_fmt yuv444p")) else { return };
    let e = probe(&p).unwrap_err();
    assert!(e.contains("4:4:4"), "{e}");
    // 10-bit needs a libx264 built with high bit depth support; skip if it isn't.
    let p = scratch("h10.mp4");
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-t", "1"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p10le"])
        .arg(&p)
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        let e = probe(&p).unwrap_err();
        assert!(e.contains("10-bit"), "{e}");
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn rejects_non_video() {
    let txt = scratch("not_video.mp4");
    std::fs::write(&txt, b"hello, this is definitely not a video file").unwrap();
    assert_eq!(probe(&txt).unwrap_err(), "not an MP4/MOV file");
    assert!(probe(&scratch("missing.mp4")).unwrap_err().contains("can't open"));
    let empty = scratch("empty.mp4");
    std::fs::write(&empty, b"").unwrap();
    assert!(probe(&empty).is_err());
    let _ = std::fs::remove_file(&txt);
    let _ = std::fs::remove_file(&empty);

    let Some(p) = fixture("audio_only.m4a", &format!("{TONE} -t 1 -c:a aac")) else { return };
    assert_eq!(probe(&p).unwrap_err(), "no video track");
}

#[test]
fn unsupported_audio_plays_silently() {
    // MP3 in MP4: video is fine, audio is ignored.
    if !have_ffmpeg() || !have_encoder("libmp3lame") {
        return;
    }
    let p =
        fixture("mp3audio.mp4", &format!("{SRC} {TONE} -t 1 -c:v libx264 -pix_fmt yuv420p -c:a libmp3lame -shortest"))
            .unwrap();
    let info = probe(&p).unwrap();
    assert!(!info.has_audio);
}

#[test]
fn truncated_file() {
    let Some(p) = high_aac() else { return };
    let data = std::fs::read(&p).unwrap();
    let cut = scratch("truncated.mp4");
    // ffmpeg puts moov at the end by default: cutting the file loses it.
    std::fs::write(&cut, &data[..data.len() / 2]).unwrap();
    assert!(probe(&cut).is_err());
    // With faststart (moov first) a truncated download keeps the frames that are present.
    let Some(fs) = fixture("faststart.mp4", &format!("{SRC} -t 3 -c:v libx264 -pix_fmt yuv420p -movflags +faststart"))
    else {
        return;
    };
    let data = std::fs::read(&fs).unwrap();
    std::fs::write(&cut, &data[..data.len() / 2]).unwrap();
    let info = probe(&cut).unwrap();
    assert!(info.duration_ms > 0 && info.duration_ms < 3000, "{info:?}");
    poster(&cut, 100).unwrap();
    let _ = std::fs::remove_file(&cut);
}

/// Decodes the whole counter clip and checks every frame comes out in presentation order with
/// the right content (B-frames with pyramid reordering).
#[test]
fn bframes_presentation_order() {
    let Some(p) = counter_clip() else { return };
    let movie = crate::demux::open(&p).unwrap();
    let v = &movie.video;
    assert!(v.track.samples.iter().any(|s| s.cts != v.track.samples[0].cts), "expected reordering in fixture");
    let mut dec = crate::video::VideoDecoder::new(&p, v).unwrap();
    let mut out = Vec::new();
    loop {
        match dec.step(v, i64::MIN).unwrap() {
            crate::video::Step::Picture(pic) => {
                let i = pic.ts as usize;
                let luma = pic.y[pic.stride_y * 32 + 32];
                out.push((v.track.pts_ms(i), luma));
            }
            crate::video::Step::Nothing => {}
            crate::video::Step::End => break,
        }
    }
    assert_eq!(out.len(), 60, "all frames decoded");
    let mut sorted = out.clone();
    sorted.sort_by_key(|x| x.0);
    // Content matches the timestamp: frame n has luma 3n (near-lossless qp).
    for (n, &(pts, luma)) in sorted.iter().enumerate() {
        assert_eq!(pts, n as i64 * 40);
        assert!((luma as i32 - (n as i32 * 3) % 256).abs() <= 2, "frame {n}: luma {luma}");
    }
    let in_order = out.windows(2).filter(|w| w[0].0 < w[1].0).count();
    eprintln!("decoder output order: {} of {} steps increasing", in_order, out.len() - 1);
}

/// Decodes from sample `start` to the end, skipping non-reference frames when `skip_nonref`;
/// returns (pts_ms, center luma) per output picture in output order.
fn decode_all(p: &Path, start: usize, skip_nonref: bool) -> (crate::demux::Movie, Vec<(i64, u8)>) {
    let movie = crate::demux::open(p).unwrap();
    let mut out = Vec::new();
    {
        let v = &movie.video;
        let mut dec = crate::video::VideoDecoder::new(p, v).unwrap();
        dec.restart_at(start).unwrap();
        loop {
            match dec.step(v, if skip_nonref { i64::MAX } else { i64::MIN }).unwrap() {
                crate::video::Step::Picture(pic) => {
                    let luma = pic.y[pic.stride_y * (pic.height / 2) + pic.width / 2];
                    out.push((v.track.pts_ms(pic.ts as usize), luma));
                }
                crate::video::Step::Nothing => {}
                crate::video::Step::End => break,
            }
        }
    }
    (movie, out)
}

/// Many x264 configurations seen in the wild (B-pyramids, open GOP, slices, CAVLC, VFR, edit
/// lists, intra refresh...). Every frame must decode, with content matching its timestamp, both
/// from the start and when starting at a later keyframe (seeking), and also when non-reference
/// frames are skipped (what the player does for late frames and while seeking).
#[test]
fn encoder_variants() {
    if !have_ffmpeg() {
        return;
    }
    let src = "-f lavfi -i nullsrc=size=96x96:rate=25,geq=lum='mod(N*3,256)':cb=128:cr=128";
    let enc = "-frames:v 50 -c:v libx264 -pix_fmt yuv420p -qp 10";
    // -itsoffset only survives remuxing a file input next to an audio stream.
    let base = fixture("v_offset_base.mp4", &format!("{src} {enc} -bf 2 -g 12")).unwrap();
    let variants: &[(&str, String)] = &[
        ("v_slower.mp4", format!("{src} {enc} -preset slower -bf 8 -refs 8 -b-pyramid normal -weightp 2 -g 20")),
        ("v_strict.mp4", format!("{src} {enc} -x264-params b-pyramid=strict:bframes=4:keyint=15")),
        ("v_opengop.mp4", format!("{src} {enc} -x264-params open-gop=1:keyint=12:bframes=3")),
        ("v_slices.mp4", format!("{src} {enc} -x264-params slices=4:keyint=25")),
        ("v_cavlc.mp4", format!("{src} {enc} -profile:v high -x264-params cabac=0:keyint=25")),
        ("v_zerolat.mp4", format!("{src} {enc} -tune zerolatency -profile:v baseline -g 10")),
        ("v_vfr.mp4", format!("{src} {enc} -vf select=lt(mod(n\\,5)\\,3) -fps_mode vfr -g 10")),
        ("v_longgop.mp4", format!("{src} {enc} -g 600 -bf 3")),
        ("v_intrarefresh.mp4", format!("{src} {enc} -x264-params intra-refresh=1:keyint=10")),
        // Video starts 0.4 s after audio: an empty edit in the video track's edit list.
        ("v_offset.mp4", format!("-itsoffset 0.4 -i {} {TONE} -c:v copy -c:a aac -t 2.4", base.display())),
        ("v_mov.mov", format!("{src} {enc} -bf 3 -g 12 -f mov")),
    ];
    for (name, args) in variants {
        let p = fixture(name, args).unwrap();
        let info = probe(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        poster(&p, 32).unwrap_or_else(|e| panic!("{name}: {e}"));

        let check = |start: usize, skip_nonref: bool| {
            let (movie, out) = decode_all(&p, start, skip_nonref);
            let t = &movie.video.track;
            let from = t.pts_ms(start);
            let mut reader = crate::demux::SampleReader::open(&p).unwrap();
            let mut keep = |i: usize| {
                let s = &t.samples[i];
                !skip_nonref || s.sync || !crate::h264::is_nonref(reader.read(s).unwrap(), movie.video.nal_len)
            };
            // Every (kept) sample presented at or after the start keyframe must come out, once.
            let mut want: Vec<i64> =
                (0..t.samples.len()).filter(|&i| keep(i)).map(|i| t.pts_ms(i)).filter(|&x| x >= from).collect();
            want.sort();
            let mut got: Vec<(i64, u8)> = out.iter().copied().filter(|x| x.0 >= from).collect();
            got.sort();
            assert_eq!(got.iter().map(|x| x.0).collect::<Vec<_>>(), want, "{name} from {start} skip {skip_nonref}");
            let offset = if name.contains("offset") { 400 } else { 0 };
            assert_eq!(t.pts_ms((0..t.samples.len()).min_by_key(|&i| t.pts(i)).unwrap()), offset, "{name}");
            for &(pts, luma) in &got {
                let n = ((pts - offset) as f64 * 25.0 / 1000.0).round() as i32;
                assert!((luma as i32 - n * 3).abs() <= 4, "{name}: frame {n} (pts {pts}) has luma {luma}");
            }
            // Output order should be presentation order (the player tolerates small deviations).
            let backwards = out.windows(2).filter(|w| w[1].0 < w[0].0).count();
            assert_eq!(backwards, 0, "{name}: {backwards} out-of-order outputs (skip {skip_nonref})");
            out.len()
        };
        let n = check(0, false);
        let kept = check(0, true);
        let movie = crate::demux::open(&p).unwrap();
        let last_key = movie.video.track.samples.iter().rposition(|s| s.sync).unwrap();
        if last_key > 0 {
            check(last_key, false);
            check(last_key, true);
        }
        eprintln!("{name}: {info:?}, {n} frames ({kept} reference), last keyframe at sample {last_key}");
    }
}
