//! Minimal ISO-BMFF (.mp4 / .m4v / .mov) demuxer.
//!
//! Only reads what playback needs: the `moov` box (track headers and sample tables) is loaded
//! into memory, sample data is read on demand from the file. Sample tables are expanded to one
//! compact entry per sample (24 bytes), so a 10 minute 30 fps clip costs ~430 KB.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::h264::{self, SpsInfo};

/// Refuse absurd `moov` boxes (corrupt or hostile files) instead of allocating them.
const MAX_MOOV: u64 = 64 << 20;
/// Refuse absurd sample counts for the same reason (~5.5 hours at 500 fps).
const MAX_SAMPLES: u32 = 10_000_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Composition offset (pts - dts) in track timescale units.
    pub cts: i32,
    /// Decode timestamp in track timescale units.
    pub dts: i64,
    pub dur: u32,
    pub sync: bool,
}

#[derive(Debug)]
pub(crate) struct Track {
    pub timescale: u32,
    pub samples: Vec<Sample>,
    /// Subtracted from `dts + cts` to get the presentation time (edit list).
    pub shift: i64,
}

impl Track {
    /// Presentation time of sample `i` in track timescale units (may be negative for pre-roll).
    pub fn pts(&self, i: usize) -> i64 {
        let s = &self.samples[i];
        s.dts + s.cts as i64 - self.shift
    }

    pub fn to_ms(&self, t: i64) -> i64 {
        (t as i128 * 1000 / self.timescale as i128) as i64
    }

    pub fn ms_to_ts(&self, ms: i64) -> i64 {
        (ms as i128 * self.timescale as i128 / 1000) as i64
    }

    pub fn pts_ms(&self, i: usize) -> i64 {
        self.to_ms(self.pts(i))
    }

    /// End of the last presented sample, in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        let end = (0..self.samples.len()).map(|i| self.pts(i) + self.samples[i].dur as i64).max().unwrap_or(0);
        self.to_ms(end).max(0) as u64
    }
}

#[derive(Debug)]
pub(crate) struct VideoTrack {
    pub track: Track,
    /// Bytes per NAL length prefix in samples (1, 2 or 4).
    pub nal_len: usize,
    /// SPS and PPS NAL units from `avcC`.
    pub param_sets: Vec<Vec<u8>>,
    pub sps: SpsInfo,
}

impl VideoTrack {
    /// Index of the last sync sample whose presentation time is <= `ms` (or the first one).
    pub fn keyframe_before(&self, ms: i64) -> usize {
        let t = self.track.ms_to_ts(ms);
        let mut best = None;
        for (i, s) in self.track.samples.iter().enumerate() {
            if !s.sync {
                continue;
            }
            if best.is_none() || self.track.pts(i) <= t {
                best = Some(i);
            } else {
                break;
            }
        }
        best.unwrap_or(0)
    }
}

#[derive(Debug)]
#[cfg_attr(not(feature = "playback"), allow(dead_code))]
pub(crate) struct AudioTrack {
    pub track: Track,
    /// AudioSpecificConfig from `esds`.
    pub asc: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug)]
pub(crate) struct Movie {
    pub video: VideoTrack,
    /// `None` if there is no audio track, or it uses a codec we can't decode (see `audio_note`).
    pub audio: Option<AudioTrack>,
    /// Why an existing audio track is ignored, if it is.
    #[allow(dead_code)]
    pub audio_note: Option<String>,
    pub duration_ms: u64,
}

/// Reads and validates the movie structure. Errors are short human-readable reasons.
pub(crate) fn open(path: &Path) -> Result<Movie, String> {
    let mut f = File::open(path).map_err(|e| format!("can't open file: {e}"))?;
    let file_len = f.metadata().map_err(|e| format!("can't read file: {e}"))?.len();
    let moov = read_moov(&mut f, file_len)?;
    parse_moov(&moov, file_len)
}

fn read_moov(f: &mut File, file_len: u64) -> Result<Vec<u8>, String> {
    let mut pos = 0u64;
    let mut first = true;
    let mut saw_moof = false;
    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos)).map_err(io)?;
        let mut hdr = [0u8; 16];
        f.read_exact(&mut hdr[..8]).map_err(io)?;
        let mut size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = hdr[4..8].try_into().unwrap();
        let mut hdr_len = 8;
        if size == 1 {
            f.read_exact(&mut hdr[8..16]).map_err(io)?;
            size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            hdr_len = 16;
        } else if size == 0 {
            size = file_len - pos;
        }
        if first {
            // Cheap sniff so random files get a clear error instead of "no moov".
            const KNOWN: [&[u8; 4]; 9] =
                [b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot", b"uuid", b"styp"];
            if !KNOWN.contains(&&kind) {
                return Err("not an MP4/MOV file".into());
            }
            first = false;
        }
        if size < hdr_len || pos + size > file_len {
            if &kind == b"moov" {
                return Err("file is truncated (incomplete moov box)".into());
            }
            break; // Truncated trailing box (e.g. mdat of an interrupted download).
        }
        match &kind {
            b"moov" => {
                let len = size - hdr_len;
                if len > MAX_MOOV {
                    return Err("moov box is too large".into());
                }
                let mut buf = vec![0u8; len as usize];
                f.read_exact(&mut buf).map_err(io)?;
                return Ok(buf);
            }
            b"moof" => saw_moof = true,
            _ => {}
        }
        pos += size;
    }
    if saw_moof {
        Err("fragmented MP4 is not supported".into())
    } else {
        Err("not a playable MP4/MOV file (no moov box)".into())
    }
}

fn io(e: std::io::Error) -> String {
    format!("read error: {e}")
}

// ---------------------------------------------------------------------------------------------
// Box parsing helpers

/// Big-endian cursor over a byte slice; every read is bounds checked.
struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Self {
        Cur { d, p: 0 }
    }
    fn left(&self) -> usize {
        self.d.len() - self.p
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.left() < n {
            return None;
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Some(s)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }
    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.bytes(4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.bytes(8).map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.d[self.p..];
        self.p = self.d.len();
        s
    }
    /// Reads a table entry count and checks that `count * entry` bytes are actually present.
    fn count(&mut self, entry: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n.checked_mul(entry)? <= self.left()).then_some(n)
    }
}

/// Iterates the child boxes of a box body.
fn boxes(d: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut p = 0usize;
    std::iter::from_fn(move || {
        if p + 8 > d.len() {
            return None;
        }
        let mut size = u32::from_be_bytes(d[p..p + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = d[p + 4..p + 8].try_into().unwrap();
        let mut hdr = 8u64;
        if size == 1 {
            if p + 16 > d.len() {
                return None;
            }
            size = u64::from_be_bytes(d[p + 8..p + 16].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = (d.len() - p) as u64;
        }
        if size < hdr || size > (d.len() - p) as u64 {
            return None;
        }
        let body = &d[p + hdr as usize..p + size as usize];
        p += size as usize;
        Some((kind, body))
    })
}

fn child<'a>(d: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(d).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn fourcc(k: &[u8]) -> String {
    k.iter()
        .map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' })
        .collect::<String>()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// moov

enum Kind {
    Video,
    Audio,
    Other,
}

fn parse_moov(moov: &[u8], file_len: u64) -> Result<Movie, String> {
    let movie_timescale = child(moov, b"mvhd")
        .and_then(|d| {
            let mut c = Cur::new(d);
            let v = c.u8()?;
            c.skip(3 + if v == 1 { 16 } else { 8 })?;
            c.u32()
        })
        .filter(|&t| t > 0)
        .unwrap_or(1000);
    let fragmented = child(moov, b"mvex").is_some();

    let mut video: Option<VideoTrack> = None;
    let mut video_err: Option<String> = None;
    let mut audio: Option<AudioTrack> = None;
    let mut audio_note: Option<String> = None;

    for (k, trak) in boxes(moov) {
        if &k != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else { continue };
        let kind = match child(mdia, b"hdlr").and_then(|d| d.get(8..12)) {
            Some(b"vide") => Kind::Video,
            Some(b"soun") => Kind::Audio,
            _ => Kind::Other,
        };
        match kind {
            Kind::Video if video.is_none() => match parse_video(trak, mdia, movie_timescale, file_len) {
                Ok(v) => video = Some(v),
                Err(e) => video_err = video_err.or(Some(e)),
            },
            Kind::Audio if audio.is_none() => match parse_audio(trak, mdia, movie_timescale, file_len) {
                Ok(a) => {
                    audio = Some(a);
                    audio_note = None;
                }
                Err(e) => audio_note = audio_note.or(Some(e)),
            },
            _ => {}
        }
    }

    let video = match (video, video_err) {
        (Some(v), _) => v,
        (None, Some(e)) => return Err(e),
        (None, None) => return Err("no video track".into()),
    };
    if video.track.samples.is_empty() {
        return Err(if fragmented {
            "fragmented MP4 is not supported".into()
        } else {
            "video track has no frames".into()
        });
    }
    if audio.as_ref().is_some_and(|a| a.track.samples.is_empty()) {
        audio = None;
    }
    let duration_ms = video.track.duration_ms().max(audio.as_ref().map_or(0, |a| a.track.duration_ms()));
    Ok(Movie { video, audio, audio_note, duration_ms })
}

struct Stbl<'a> {
    stsd: &'a [u8],
    track: Track,
}

fn parse_track<'a>(trak: &'a [u8], mdia: &'a [u8], movie_ts: u32, file_len: u64) -> Result<Stbl<'a>, String> {
    let bad = |what: &str| format!("invalid MP4 ({what})");
    let timescale = child(mdia, b"mdhd")
        .and_then(|d| {
            let mut c = Cur::new(d);
            let v = c.u8()?;
            c.skip(3 + if v == 1 { 16 } else { 8 })?;
            c.u32()
        })
        .filter(|&t| t > 0)
        .ok_or_else(|| bad("mdhd"))?;
    let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).ok_or_else(|| bad("no stbl"))?;
    let stsd = child(stbl, b"stsd").ok_or_else(|| bad("no stsd"))?;

    // Edit list: leading empty edits delay the track, the first real edit says where media starts.
    let mut shift = 0i64;
    if let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) {
        let mut c = Cur::new(elst);
        let v = c.u8().unwrap_or(0);
        let _ = c.skip(3);
        let entry = if v == 1 { 20 } else { 12 };
        let n = c.count(entry).unwrap_or(0);
        let mut delay = 0i64; // movie timescale
        for _ in 0..n {
            let (dur, media_time) = if v == 1 {
                (c.u64().unwrap_or(0) as i64, c.u64().unwrap_or(0) as i64)
            } else {
                (c.u32().unwrap_or(0) as i64, c.u32().unwrap_or(0) as i32 as i64)
            };
            let _ = c.skip(4);
            if media_time == -1 {
                delay = delay.saturating_add(dur);
            } else {
                let delay_ts = (delay as i128 * timescale as i128 / movie_ts as i128) as i64;
                shift = media_time - delay_ts;
                break;
            }
        }
    }

    let samples = build_samples(stbl, file_len)?;
    Ok(Stbl { stsd, track: Track { timescale, samples, shift } })
}

fn build_samples(stbl: &[u8], file_len: u64) -> Result<Vec<Sample>, String> {
    let bad = |what: &str| format!("invalid MP4 sample table ({what})");

    // Sizes
    let mut sizes: Vec<u32> = Vec::new();
    if let Some(d) = child(stbl, b"stsz") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("stsz"))?;
        let fixed = c.u32().ok_or_else(|| bad("stsz"))?;
        let n = c.u32().ok_or_else(|| bad("stsz"))?;
        if n > MAX_SAMPLES {
            return Err("too many samples".into());
        }
        if fixed != 0 {
            sizes = vec![fixed; n as usize];
        } else {
            if (n as usize) * 4 > c.left() {
                return Err(bad("stsz"));
            }
            sizes = (0..n).map(|_| c.u32().unwrap()).collect();
        }
    } else if let Some(d) = child(stbl, b"stz2") {
        let mut c = Cur::new(d);
        c.skip(7).ok_or_else(|| bad("stz2"))?;
        let field = c.u8().ok_or_else(|| bad("stz2"))?;
        let n = c.u32().ok_or_else(|| bad("stz2"))?;
        if n > MAX_SAMPLES {
            return Err("too many samples".into());
        }
        let raw = c.rest();
        let need = (n as usize * field as usize).div_ceil(8);
        if raw.len() < need || !matches!(field, 4 | 8 | 16) {
            return Err(bad("stz2"));
        }
        sizes = (0..n as usize)
            .map(|i| match field {
                4 => ((raw[i / 2] >> if i % 2 == 0 { 4 } else { 0 }) & 0xf) as u32,
                8 => raw[i] as u32,
                _ => u16::from_be_bytes([raw[2 * i], raw[2 * i + 1]]) as u32,
            })
            .collect();
    }
    let n = sizes.len();
    if n == 0 {
        return Ok(Vec::new());
    }

    // Chunk offsets
    let offsets: Vec<u64> = if let Some(d) = child(stbl, b"stco") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("stco"))?;
        let k = c.count(4).ok_or_else(|| bad("stco"))?;
        (0..k).map(|_| c.u32().unwrap() as u64).collect()
    } else if let Some(d) = child(stbl, b"co64") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("co64"))?;
        let k = c.count(8).ok_or_else(|| bad("co64"))?;
        (0..k).map(|_| c.u64().unwrap()).collect()
    } else {
        return Err(bad("no chunk offsets"));
    };

    // Sample-to-chunk
    let stsc: Vec<(u32, u32)> = {
        let d = child(stbl, b"stsc").ok_or_else(|| bad("no stsc"))?;
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("stsc"))?;
        let k = c.count(12).ok_or_else(|| bad("stsc"))?;
        (0..k)
            .map(|_| {
                let first = c.u32().unwrap();
                let per = c.u32().unwrap();
                c.skip(4);
                (first, per)
            })
            .collect()
    };

    let mut samples = Vec::with_capacity(n);
    let mut si = 0usize;
    'chunks: for (ci, &chunk_off) in offsets.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        // Last stsc run starting at or before this chunk.
        let per = match stsc.iter().rev().find(|(first, _)| *first <= chunk_no) {
            Some((_, per)) => *per,
            None => return Err(bad("stsc")),
        };
        let mut off = chunk_off;
        for _ in 0..per {
            if si >= n {
                break 'chunks;
            }
            let size = sizes[si];
            if off.saturating_add(size as u64) > file_len {
                // Truncated file: keep what is fully present.
                break 'chunks;
            }
            samples.push(Sample { offset: off, size, cts: 0, dts: 0, dur: 0, sync: true });
            off += size as u64;
            si += 1;
        }
    }
    let n = samples.len();

    // Decode times
    if let Some(d) = child(stbl, b"stts") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("stts"))?;
        let k = c.count(8).ok_or_else(|| bad("stts"))?;
        let mut i = 0usize;
        let mut t = 0i64;
        for _ in 0..k {
            let count = c.u32().unwrap() as usize;
            let delta = c.u32().unwrap();
            for _ in 0..count {
                if i >= n {
                    break;
                }
                samples[i].dts = t;
                samples[i].dur = delta;
                t += delta as i64;
                i += 1;
            }
        }
        // Missing entries: continue with the last delta.
        let last = if i > 0 { samples[i - 1].dur } else { 1 };
        while i < n {
            samples[i].dts = t;
            samples[i].dur = last;
            t += last as i64;
            i += 1;
        }
    } else {
        return Err(bad("no stts"));
    }

    // Composition offsets (B-frames). Version 0 is nominally unsigned but writers use it signed.
    if let Some(d) = child(stbl, b"ctts") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("ctts"))?;
        let k = c.count(8).ok_or_else(|| bad("ctts"))?;
        let mut i = 0usize;
        for _ in 0..k {
            let count = c.u32().unwrap() as usize;
            let off = c.u32().unwrap() as i32;
            for _ in 0..count {
                if i >= n {
                    break;
                }
                samples[i].cts = off;
                i += 1;
            }
        }
    }

    // Sync samples; absent table means every sample is a sync sample.
    if let Some(d) = child(stbl, b"stss") {
        let mut c = Cur::new(d);
        c.skip(4).ok_or_else(|| bad("stss"))?;
        let k = c.count(4).ok_or_else(|| bad("stss"))?;
        for s in samples.iter_mut() {
            s.sync = false;
        }
        for _ in 0..k {
            let no = c.u32().unwrap() as usize;
            if no >= 1 && no <= n {
                samples[no - 1].sync = true;
            }
        }
        if !samples.iter().any(|s| s.sync) {
            samples[0].sync = true;
        }
    }
    Ok(samples)
}

/// First sample entry of `stsd`: (format fourcc, entry body after the 8-byte header).
fn sample_entry(stsd: &[u8]) -> Option<([u8; 4], &[u8])> {
    let mut c = Cur::new(stsd);
    c.skip(4)?;
    let n = c.u32()?;
    if n == 0 {
        return None;
    }
    boxes(c.rest()).next()
}

fn parse_video(trak: &[u8], mdia: &[u8], movie_ts: u32, file_len: u64) -> Result<VideoTrack, String> {
    let Stbl { stsd, track } = parse_track(trak, mdia, movie_ts, file_len)?;
    let (fmt, entry) = sample_entry(stsd).ok_or("invalid MP4 (empty stsd)")?;
    match &fmt {
        b"avc1" | b"avc3" => {}
        b"hvc1" | b"hev1" => return Err("HEVC (H.265) video is not supported, only H.264".into()),
        b"vp09" | b"vp08" => return Err("VP8/VP9 video is not supported, only H.264".into()),
        b"av01" => return Err("AV1 video is not supported, only H.264".into()),
        b"mp4v" => return Err("MPEG-4 Part 2 video is not supported, only H.264".into()),
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => {
            return Err("ProRes video is not supported, only H.264".into());
        }
        other => return Err(format!("video codec '{}' is not supported, only H.264", fourcc(other))),
    }
    // VisualSampleEntry fields are 78 bytes, then child boxes.
    let children = entry.get(78..).ok_or("invalid MP4 (short avc1 entry)")?;
    let avcc = child(children, b"avcC").ok_or("invalid MP4 (missing avcC)")?;
    let mut c = Cur::new(avcc);
    let bad = || "invalid MP4 (bad avcC)".to_string();
    c.skip(4).ok_or_else(bad)?; // version, profile, compat, level
    let nal_len = (c.u8().ok_or_else(bad)? & 3) as usize + 1;
    if nal_len == 3 {
        return Err(bad());
    }
    let mut param_sets = Vec::new();
    let nsps = c.u8().ok_or_else(bad)? & 0x1f;
    let mut sps_info = None;
    for _ in 0..nsps {
        let len = c.u16().ok_or_else(bad)? as usize;
        let nal = c.bytes(len).ok_or_else(bad)?;
        if sps_info.is_none() {
            sps_info = Some(h264::parse_sps(nal)?);
        }
        param_sets.push(nal.to_vec());
    }
    let npps = c.u8().ok_or_else(bad)?;
    for _ in 0..npps {
        let len = c.u16().ok_or_else(bad)? as usize;
        param_sets.push(c.bytes(len).ok_or_else(bad)?.to_vec());
    }
    let sps = match sps_info {
        Some(s) => s,
        None if &fmt == b"avc3" => {
            return Err("H.264 without out-of-band parameter sets (avc3) is not supported".into());
        }
        None => return Err(bad()),
    };
    sps.check_supported()?;
    Ok(VideoTrack { track, nal_len, param_sets, sps })
}

fn parse_audio(trak: &[u8], mdia: &[u8], movie_ts: u32, file_len: u64) -> Result<AudioTrack, String> {
    let Stbl { stsd, track } = parse_track(trak, mdia, movie_ts, file_len)?;
    let (fmt, entry) = sample_entry(stsd).ok_or("invalid audio track")?;
    if &fmt != b"mp4a" {
        return Err(format!("audio codec '{}' is not supported, only AAC", fourcc(&fmt)));
    }
    let mut c = Cur::new(entry);
    let bad = || "invalid MP4 (bad mp4a entry)".to_string();
    c.skip(8).ok_or_else(bad)?;
    let version = c.u16().ok_or_else(bad)?;
    c.skip(6).ok_or_else(bad)?;
    let mut channels = c.u16().ok_or_else(bad)?;
    c.skip(6).ok_or_else(bad)?;
    let mut sample_rate = c.u32().ok_or_else(bad)? >> 16;
    match version {
        1 => c.skip(16).ok_or_else(bad)?,
        2 => {
            c.skip(4).ok_or_else(bad)?;
            sample_rate = f64::from_bits(c.u64().ok_or_else(bad)?) as u32;
            channels = c.u32().ok_or_else(bad)? as u16;
            c.skip(20).ok_or_else(bad)?;
        }
        _ => {}
    }
    let children = c.rest();
    let esds = child(children, b"esds")
        .or_else(|| child(children, b"wave").and_then(|w| child(w, b"esds")))
        .ok_or("audio track has no esds")?;
    let (oti, asc) = parse_esds(esds).ok_or("invalid MP4 (bad esds)")?;
    match oti {
        0x40 | 0x66 | 0x67 | 0x68 => {}
        0x69 | 0x6b => return Err("MP3 audio in MP4 is not supported, only AAC".into()),
        o => return Err(format!("audio object type 0x{o:02x} is not supported, only AAC")),
    }
    let (asc_rate, asc_channels) = check_asc(&asc)?;
    Ok(AudioTrack {
        track,
        asc,
        sample_rate: if asc_rate > 0 { asc_rate } else { sample_rate },
        channels: if asc_channels > 0 { asc_channels } else { channels },
    })
}

/// Returns (objectTypeIndication, DecoderSpecificInfo).
fn parse_esds(d: &[u8]) -> Option<(u8, Vec<u8>)> {
    fn desc<'a>(c: &mut Cur<'a>) -> Option<(u8, &'a [u8])> {
        let tag = c.u8()?;
        let mut len = 0usize;
        for _ in 0..4 {
            let b = c.u8()?;
            len = (len << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        Some((tag, c.bytes(len.min(c.left()))?))
    }
    let mut c = Cur::new(d);
    c.skip(4)?;
    let (tag, es) = desc(&mut c)?;
    if tag != 3 {
        return None;
    }
    let mut c = Cur::new(es);
    c.skip(2)?;
    let flags = c.u8()?;
    if flags & 0x80 != 0 {
        c.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let n = c.u8()? as usize;
        c.skip(n)?;
    }
    if flags & 0x20 != 0 {
        c.skip(2)?;
    }
    while c.left() > 0 {
        let (tag, body) = desc(&mut c)?;
        if tag == 4 {
            let mut d = Cur::new(body);
            let oti = d.u8()?;
            d.skip(12)?;
            while d.left() > 0 {
                let (t, b) = desc(&mut d)?;
                if t == 5 {
                    return Some((oti, b.to_vec()));
                }
            }
            return Some((oti, Vec::new()));
        }
    }
    None
}

/// Checks that the AudioSpecificConfig is AAC-LC with at most 2 channels (what the decoder
/// supports). Returns (sample_rate, channels), 0 meaning "not stated".
fn check_asc(asc: &[u8]) -> Result<(u32, u16), String> {
    if asc.len() < 2 {
        return Err("AAC track without decoder config".into());
    }
    let mut b = h264::Bits::new(asc);
    let mut ot = b.u(5).ok_or("bad AAC config")?;
    if ot == 31 {
        ot = 32 + b.u(6).ok_or("bad AAC config")?;
    }
    let freq_idx = b.u(4).ok_or("bad AAC config")?;
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let rate = if freq_idx == 15 {
        b.u(24).ok_or("bad AAC config")?
    } else {
        RATES.get(freq_idx as usize).copied().unwrap_or(0)
    };
    let ch = b.u(4).ok_or("bad AAC config")? as u16;
    match ot {
        2 => {}
        5 | 29 => return Err("HE-AAC audio is not supported, only AAC-LC".into()),
        o => return Err(format!("AAC object type {o} is not supported, only AAC-LC")),
    }
    if ch > 2 {
        return Err("surround (more than 2 channel) AAC audio is not supported".into());
    }
    if ch == 0 {
        return Err("AAC with a program config element is not supported".into());
    }
    Ok((rate, ch))
}

// ---------------------------------------------------------------------------------------------
// Sample reading

/// Reads sample payloads from a file, reusing one buffer.
pub(crate) struct SampleReader {
    file: File,
    buf: Vec<u8>,
}

impl SampleReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        Ok(SampleReader { file: File::open(path).map_err(|e| format!("can't open file: {e}"))?, buf: Vec::new() })
    }

    pub fn read(&mut self, s: &Sample) -> Result<&[u8], String> {
        self.buf.resize(s.size as usize, 0);
        self.file.seek(SeekFrom::Start(s.offset)).map_err(io)?;
        self.file.read_exact(&mut self.buf).map_err(io)?;
        Ok(&self.buf)
    }
}
