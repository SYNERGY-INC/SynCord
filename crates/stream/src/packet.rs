//! Datagram framing for encoded video: [`packetize`] splits a frame into datagrams of bounded
//! size, [`Reassembler`] puts them back together on the receiving side, tolerating reordering,
//! duplicates and loss, and tells when the decoder needs a keyframe.
//!
//! # Wire format
//!
//! Every datagram is an 18-byte header followed by an opaque slice of the encoded frame.
//! All integers are big-endian.
//!
//! ```text
//! offset size field
//!   0     1   version (1)
//!   1     1   flags: bit 0 = keyframe (the frame is an IDR picture), other bits reserved (0)
//!   2     4   stream id
//!   6     4   frame number (increments by one per encoded frame, wraps)
//!  10     2   fragment index (0-based)
//!  12     2   fragment count (>= 1)
//!  14     4   timestamp in milliseconds (sender clock, wraps)
//!  18     ..  payload: bytes [start, end) of the frame, fragments in index order concatenate
//!             to the whole frame
//! ```
//!
//! Fragments of one frame have (almost) equal sizes, so a frame never ends with a tiny packet.

use std::time::{Duration, Instant};

/// Size of the header in front of every fragment.
pub const HEADER_LEN: usize = 18;
/// Default datagram size limit: fits a 1280-byte IPv6 minimum MTU after IP/UDP headers and the
/// relay's own encryption overhead.
pub const DEFAULT_MAX_PACKET: usize = 1150;
/// Current wire format version.
pub const VERSION: u8 = 1;
/// Flag bit: the fragment belongs to a keyframe.
pub const FLAG_KEYFRAME: u8 = 1;

/// Parsed fragment header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    pub stream_id: u32,
    pub frame: u32,
    pub index: u16,
    pub count: u16,
    pub keyframe: bool,
    pub timestamp_ms: u32,
}

impl PacketHeader {
    /// Parses a datagram into its header and payload. `None` for anything malformed (short,
    /// unknown version, index out of range).
    pub fn parse(datagram: &[u8]) -> Option<(PacketHeader, &[u8])> {
        if datagram.len() < HEADER_LEN || datagram[0] != VERSION {
            return None;
        }
        let d = datagram;
        let u32_at = |i: usize| u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        let u16_at = |i: usize| u16::from_be_bytes([d[i], d[i + 1]]);
        let h = PacketHeader {
            keyframe: d[1] & FLAG_KEYFRAME != 0,
            stream_id: u32_at(2),
            frame: u32_at(6),
            index: u16_at(10),
            count: u16_at(12),
            timestamp_ms: u32_at(14),
        };
        if h.count == 0 || h.index >= h.count {
            return None;
        }
        Some((h, &d[HEADER_LEN..]))
    }

    /// Appends the 18 header bytes to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.push(if self.keyframe { FLAG_KEYFRAME } else { 0 });
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.frame.to_be_bytes());
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
    }
}

/// Reads just the stream id of a datagram (for routing), without validating the rest.
pub fn stream_id_of(datagram: &[u8]) -> Option<u32> {
    PacketHeader::parse(datagram).map(|(h, _)| h.stream_id)
}

/// Splits one encoded frame into datagrams of at most `max_packet` bytes (header included) and
/// calls `emit` for each, in index order. Returns the number of datagrams.
///
/// Errors if `max_packet` leaves no room for payload or the frame would need more than 65535
/// fragments.
pub fn packetize(
    stream_id: u32,
    frame: u32,
    keyframe: bool,
    timestamp_ms: u32,
    data: &[u8],
    max_packet: usize,
    mut emit: impl FnMut(&[u8]),
) -> Result<usize, String> {
    let room = max_packet.saturating_sub(HEADER_LEN);
    if room == 0 {
        return Err(format!("packet size {max_packet} leaves no room for payload"));
    }
    let count = data.len().div_ceil(room).max(1);
    if count > u16::MAX as usize {
        return Err(format!("frame of {} bytes needs too many packets", data.len()));
    }
    let mut buf = Vec::with_capacity(max_packet);
    let (base, extra) = (data.len() / count, data.len() % count);
    let mut start = 0;
    for index in 0..count {
        // The first `extra` fragments carry one more byte, so sizes differ by at most one.
        let len = base + usize::from(index < extra);
        buf.clear();
        PacketHeader { stream_id, frame, index: index as u16, count: count as u16, keyframe, timestamp_ms }
            .write(&mut buf);
        buf.extend_from_slice(&data[start..start + len]);
        start += len;
        emit(&buf);
    }
    Ok(count)
}

/// A frame put back together by the [`Reassembler`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteFrame {
    pub frame: u32,
    pub keyframe: bool,
    pub timestamp_ms: u32,
    pub data: Vec<u8>,
}

/// Reassembler limits.
#[derive(Clone, Debug)]
pub struct ReassemblerConfig {
    /// How long a missing frame is waited for (counted from when the first packet of it, or of
    /// any later frame, arrived) before it is declared lost.
    pub frame_timeout: Duration,
    /// Frames in flight at most; the oldest is dropped beyond this.
    pub max_frames: usize,
    /// Buffered payload bytes at most.
    pub max_bytes: usize,
}

impl Default for ReassemblerConfig {
    fn default() -> Self {
        ReassemblerConfig { frame_timeout: Duration::from_millis(200), max_frames: 48, max_bytes: 8 << 20 }
    }
}

/// Why a datagram was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Not a valid packet.
    Malformed,
    /// Belongs to another stream.
    OtherStream,
    /// The frame was already delivered or given up on (late duplicate).
    Late,
}

/// Counters, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReassemblerStats {
    pub packets: u64,
    pub duplicates: u64,
    pub late: u64,
    pub frames_delivered: u64,
    /// Frames that never completed (or were skipped while waiting for a keyframe).
    pub frames_lost: u64,
}

struct Partial {
    frame: u32,
    keyframe: bool,
    timestamp_ms: u32,
    frags: Vec<Option<Vec<u8>>>,
    have: usize,
    bytes: usize,
    first_seen: Instant,
}

impl Partial {
    fn complete(&self) -> bool {
        self.have == self.frags.len()
    }
}

/// Signed distance from `b` to `a` in frame numbers (wrapping).
fn diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

/// Frames this far behind what we expect are treated as a sender restart, not as late packets.
const RESTART_DISTANCE: i32 = 1000;

/// Puts fragments back together and releases complete frames strictly in order, so every frame
/// handed out can be decoded. When a frame is lost, the following non-keyframes are useless to a
/// decoder: they are dropped and [`Reassembler::needs_keyframe`] turns true until a keyframe has
/// been delivered. Pure state machine; time is passed in.
pub struct Reassembler {
    stream_id: u32,
    config: ReassemblerConfig,
    partial: Vec<Partial>,
    bytes: usize,
    /// Next frame to deliver; `None` while waiting for a keyframe.
    next: Option<u32>,
    /// Last frame delivered or given up on, to recognise late packets.
    last_done: Option<u32>,
    need_keyframe: bool,
    stats: ReassemblerStats,
}

impl Reassembler {
    pub fn new(stream_id: u32, config: ReassemblerConfig) -> Reassembler {
        Reassembler {
            stream_id,
            config,
            partial: Vec::new(),
            bytes: 0,
            next: None,
            last_done: None,
            need_keyframe: false,
            stats: ReassemblerStats::default(),
        }
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// `true` when frames were lost (or we joined mid-stream) and nothing decodable will arrive
    /// until the sender produces a keyframe: ask it for one.
    pub fn needs_keyframe(&self) -> bool {
        self.need_keyframe
    }

    pub fn stats(&self) -> ReassemblerStats {
        self.stats
    }

    /// Forgets everything and waits for a keyframe (e.g. after a decode error).
    pub fn reset(&mut self) {
        self.stats.frames_lost += self.partial.len() as u64;
        self.partial.clear();
        self.bytes = 0;
        self.next = None;
        self.need_keyframe = true;
    }

    /// Adds one datagram.
    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Result<(), PushError> {
        let (h, payload) = PacketHeader::parse(datagram).ok_or(PushError::Malformed)?;
        if h.stream_id != self.stream_id {
            return Err(PushError::OtherStream);
        }
        self.stats.packets += 1;
        let behind = |n: u32| diff(h.frame, n);
        let late = match (self.next, self.last_done) {
            (Some(n), _) => behind(n) < 0,
            (None, Some(d)) => behind(d) <= 0,
            (None, None) => false,
        };
        if late {
            let reference = self.next.or(self.last_done).unwrap_or(h.frame);
            if behind(reference) < -RESTART_DISTANCE {
                // The sender started over (new encoder, counter reset): start over too.
                self.partial.clear();
                self.bytes = 0;
                self.next = None;
                self.last_done = None;
            } else {
                self.stats.late += 1;
                return Err(PushError::Late);
            }
        }
        let idx = match self.partial.iter().position(|p| p.frame == h.frame) {
            Some(i) => i,
            None => {
                self.partial.push(Partial {
                    frame: h.frame,
                    keyframe: h.keyframe,
                    timestamp_ms: h.timestamp_ms,
                    frags: vec![None; h.count as usize],
                    have: 0,
                    bytes: 0,
                    first_seen: now,
                });
                self.partial.len() - 1
            }
        };
        let p = &mut self.partial[idx];
        if p.frags.len() != h.count as usize || p.keyframe != h.keyframe {
            // Inconsistent with earlier fragments of this frame: corrupt, ignore.
            return Err(PushError::Malformed);
        }
        let slot = &mut p.frags[h.index as usize];
        if slot.is_some() {
            self.stats.duplicates += 1;
            return Ok(());
        }
        *slot = Some(payload.to_vec());
        p.have += 1;
        p.bytes += payload.len();
        self.bytes += payload.len();
        self.enforce_limits();
        Ok(())
    }

    fn enforce_limits(&mut self) {
        while self.partial.len() > self.config.max_frames || self.bytes > self.config.max_bytes {
            // Drop the oldest frame (by number relative to the newest).
            let Some(newest) = self.partial.iter().map(|p| p.frame).max_by(|&a, &b| diff(a, b).cmp(&0)) else {
                break;
            };
            let Some(i) = (0..self.partial.len()).max_by_key(|&i| diff(newest, self.partial[i].frame)) else {
                break;
            };
            let p = self.partial.swap_remove(i);
            self.bytes -= p.bytes;
            self.give_up(p.frame);
        }
    }

    /// Marks `frame` as lost: the decode chain is broken until a keyframe.
    fn give_up(&mut self, frame: u32) {
        self.stats.frames_lost += 1;
        if self.next.is_some_and(|n| diff(frame, n) >= 0) || self.next.is_none() {
            self.next = None;
            self.need_keyframe = true;
        }
        if self.last_done.is_none_or(|d| diff(frame, d) > 0) {
            self.last_done = Some(frame);
        }
    }

    fn take(&mut self, i: usize) -> CompleteFrame {
        let p = self.partial.swap_remove(i);
        self.bytes -= p.bytes;
        let mut data = Vec::with_capacity(p.bytes);
        for f in p.frags.into_iter().flatten() {
            data.extend_from_slice(&f);
        }
        self.stats.frames_delivered += 1;
        self.last_done = Some(p.frame);
        self.next = Some(p.frame.wrapping_add(1));
        if p.keyframe {
            self.need_keyframe = false;
        }
        CompleteFrame { frame: p.frame, keyframe: p.keyframe, timestamp_ms: p.timestamp_ms, data }
    }

    /// Drops buffered frames older than `frame`; returns how many.
    fn drop_before(&mut self, frame: u32) -> u64 {
        let before = self.partial.len();
        self.partial.retain(|p| diff(p.frame, frame) >= 0);
        self.bytes = self.partial.iter().map(|p| p.bytes).sum();
        (before - self.partial.len()) as u64
    }

    /// Returns the next frame that can be decoded, if any, applying timeouts as of `now`. Call it
    /// in a loop after each [`push`](Self::push) until it returns `None`.
    pub fn pop(&mut self, now: Instant) -> Option<CompleteFrame> {
        loop {
            // The newest complete keyframe resolves everything before it.
            let key = self
                .partial
                .iter()
                .enumerate()
                .filter(|(_, p)| p.keyframe && p.complete())
                .max_by(|a, b| diff(a.1.frame, b.1.frame).cmp(&0))
                .map(|(i, p)| (i, p.frame));
            match self.next {
                Some(n) => {
                    if let Some(i) = self.partial.iter().position(|p| p.frame == n && p.complete()) {
                        return Some(self.take(i));
                    }
                    if let Some((_, k)) = key.filter(|&(_, k)| diff(k, n) > 0) {
                        // Skip straight to a newer keyframe instead of waiting for the gap.
                        self.drop_before(k);
                        self.stats.frames_lost += diff(k, n) as u64;
                        let i = self.partial.iter().position(|p| p.frame == k).unwrap();
                        return Some(self.take(i));
                    }
                    // Is frame `n` overdue? Waiting starts when it, or anything after it, showed up.
                    let waiting_since =
                        self.partial.iter().filter(|p| diff(p.frame, n) >= 0).map(|p| p.first_seen).min();
                    match waiting_since {
                        Some(t) if now.saturating_duration_since(t) >= self.config.frame_timeout => {
                            // Lost. Everything up to the next keyframe is undecodable.
                            if let Some(i) = self.partial.iter().position(|p| p.frame == n) {
                                let p = self.partial.swap_remove(i);
                                self.bytes -= p.bytes;
                            }
                            self.give_up(n);
                            continue;
                        }
                        _ => return None,
                    }
                }
                None => {
                    if let Some((i, k)) = key {
                        self.stats.frames_lost += self.drop_before(k);
                        let i = self.partial.iter().position(|p| p.frame == k).unwrap_or(i);
                        return Some(self.take(i));
                    }
                    // Complete non-keyframes cannot be decoded without the keyframe before them.
                    // Unless such a keyframe is still being received, the sender has to send a new
                    // one. They are kept for a moment in case a reordered keyframe shows up.
                    let pending_key =
                        self.partial.iter().filter(|p| p.keyframe).map(|p| p.frame).min_by(|&a, &b| diff(a, b).cmp(&0));
                    let useless = |p: &Partial| !p.keyframe && pending_key.is_none_or(|k| diff(p.frame, k) < 0);
                    if self.partial.iter().any(|p| useless(p) && p.complete()) {
                        self.need_keyframe = true;
                    }
                    // Drop what is useless or never completed, once timed out.
                    let timeout = self.config.frame_timeout;
                    let before = self.partial.len();
                    self.partial.retain(|p| {
                        let age = now.saturating_duration_since(p.first_seen);
                        !((useless(p) && age >= timeout) || age >= timeout * 4)
                    });
                    let dropped = before - self.partial.len();
                    if dropped > 0 {
                        self.stats.frames_lost += dropped as u64;
                        self.bytes = self.partial.iter().map(|p| p.bytes).sum();
                        self.need_keyframe = true;
                    }
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic PRNG (xorshift) so tests need no extra crates.
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
        fn shuffle<T>(&mut self, v: &mut [T]) {
            for i in (1..v.len()).rev() {
                v.swap(i, self.below(i + 1));
            }
        }
    }

    fn frame_bytes(n: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u32).wrapping_mul(31).wrapping_add(n * 7) as u8).collect()
    }

    fn packets(frame: u32, key: bool, len: usize, max: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        packetize(9, frame, key, frame * 33, &frame_bytes(frame, len), max, |p| out.push(p.to_vec())).unwrap();
        out
    }

    fn drain(r: &mut Reassembler, now: Instant) -> Vec<CompleteFrame> {
        std::iter::from_fn(|| r.pop(now)).collect()
    }

    #[test]
    fn header_roundtrip_and_rejects() {
        let h = PacketHeader {
            stream_id: 0xdead_beef,
            frame: 77,
            index: 3,
            count: 9,
            keyframe: true,
            timestamp_ms: 123456,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert_eq!(buf.len(), HEADER_LEN);
        buf.extend_from_slice(b"xyz");
        assert_eq!(PacketHeader::parse(&buf), Some((h, &b"xyz"[..])));
        assert_eq!(stream_id_of(&buf), Some(0xdead_beef));
        assert_eq!(PacketHeader::parse(&buf[..10]), None);
        let mut bad = buf.clone();
        bad[0] = 2;
        assert_eq!(PacketHeader::parse(&bad), None);
        let mut bad = buf.clone();
        bad[11] = 9; // index == count
        assert_eq!(PacketHeader::parse(&bad), None);
    }

    #[test]
    fn packetize_sizes() {
        for len in [0usize, 1, 1131, 1132, 1133, 5000, 200_000] {
            let p = packets(1, false, len, DEFAULT_MAX_PACKET);
            assert!(p.iter().all(|d| d.len() <= DEFAULT_MAX_PACKET));
            assert_eq!(p.len(), len.div_ceil(DEFAULT_MAX_PACKET - HEADER_LEN).max(1));
            let sizes: Vec<usize> = p.iter().map(|d| d.len()).collect();
            assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 1, "{sizes:?}");
            let joined: Vec<u8> = p.iter().flat_map(|d| d[HEADER_LEN..].to_vec()).collect();
            assert_eq!(joined, frame_bytes(1, len));
        }
        assert!(packetize(1, 1, false, 0, &[1, 2], HEADER_LEN, |_| {}).is_err());
        assert!(packetize(1, 1, false, 0, &vec![0; 70_000], HEADER_LEN + 1, |_| {}).is_err());
    }

    #[test]
    fn in_order_and_shuffled_with_duplicates() {
        let t = Instant::now();
        let mut rng = Rng(0x1234_5678);
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        let mut all = Vec::new();
        for f in 0..40u32 {
            for p in packets(f, f % 10 == 0, 500 + rng.below(6000), 600) {
                all.push(p.clone());
                if rng.below(4) == 0 {
                    all.push(p); // duplicate
                }
            }
        }
        // Reorder within a sliding window of ~3 frames' worth of packets.
        for chunk in all.chunks_mut(30) {
            rng.shuffle(chunk);
        }
        let mut got = Vec::new();
        for p in &all {
            let _ = r.push(p, t);
            got.extend(drain(&mut r, t));
        }
        let nums: Vec<u32> = got.iter().map(|f| f.frame).collect();
        assert_eq!(nums, (0..40).collect::<Vec<_>>());
        for f in &got {
            assert_eq!(&f.data[..20], &frame_bytes(f.frame, 20)[..]);
            assert_eq!(f.timestamp_ms, f.frame * 33);
        }
        assert!(!r.needs_keyframe());
        assert!(r.stats().duplicates > 0);
        assert_eq!(r.bytes, 0);
    }

    #[test]
    fn loss_waits_for_keyframe() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        let mut delivered = Vec::new();
        for f in 0..20u32 {
            let mut ps = packets(f, f == 0 || f == 15, 3000, 1150);
            if f == 5 {
                ps.remove(1); // one fragment of frame 5 lost
            }
            for p in ps {
                r.push(&p, ms(f as u64 * 33)).unwrap_or(());
            }
            delivered.extend(drain(&mut r, ms(f as u64 * 33)).into_iter().map(|c| c.frame));
            if (6..15).contains(&f) {
                // Frame 5 is declared lost 200 ms after it started arriving.
                assert_eq!(r.needs_keyframe(), f as u64 * 33 >= 5 * 33 + 200, "frame {f}");
            }
        }
        // 0..4 delivered, 5 lost after the timeout, 6..14 useless, keyframe 15 resumes.
        assert_eq!(delivered, [0, 1, 2, 3, 4, 15, 16, 17, 18, 19]);
        assert!(!r.needs_keyframe());
        assert!(r.stats().frames_lost >= 7);
    }

    #[test]
    fn whole_frame_missing_is_detected_by_later_frames() {
        let t0 = Instant::now();
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        for p in packets(0, true, 2000, 1150) {
            r.push(&p, t0).unwrap();
        }
        assert_eq!(drain(&mut r, t0).len(), 1);
        // Frame 1 never arrives; frame 2 does.
        for p in packets(2, false, 2000, 1150) {
            r.push(&p, t0).unwrap();
        }
        assert!(drain(&mut r, t0).is_empty());
        assert!(!r.needs_keyframe()); // still within the reorder window
        // Late frame 1 within the window is fine.
        let late = t0 + Duration::from_millis(100);
        for p in packets(1, false, 2000, 1150) {
            r.push(&p, late).unwrap();
        }
        assert_eq!(drain(&mut r, late).iter().map(|f| f.frame).collect::<Vec<_>>(), [1, 2]);
        // Now frame 3 lost for good.
        for p in packets(4, false, 2000, 1150) {
            r.push(&p, late).unwrap();
        }
        let later = late + Duration::from_millis(250);
        assert!(drain(&mut r, later).is_empty());
        assert!(r.needs_keyframe());
        // Late packets of the lost frame are rejected.
        assert_eq!(r.push(&packets(3, false, 2000, 1150)[0], later), Err(PushError::Late));
    }

    #[test]
    fn joining_mid_stream_requests_keyframe() {
        let t = Instant::now();
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        assert!(!r.needs_keyframe());
        for p in packets(50, false, 900, 1150) {
            r.push(&p, t).unwrap();
        }
        assert!(drain(&mut r, t).is_empty());
        assert!(r.needs_keyframe());
        // A keyframe whose packets arrive shuffled resumes the stream.
        let mut ps = packets(51, true, 9000, 1150);
        ps.reverse();
        for p in ps {
            r.push(&p, t).unwrap();
        }
        let got = drain(&mut r, t);
        assert_eq!(got.len(), 1);
        assert!(got[0].keyframe && got[0].data == frame_bytes(51, 9000));
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn newer_keyframe_skips_gap_and_other_streams_ignored() {
        let t = Instant::now();
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        for f in [0u32, 1] {
            for p in packets(f, f == 0, 100, 1150) {
                r.push(&p, t).unwrap();
            }
        }
        assert_eq!(drain(&mut r, t).len(), 2);
        // 2 and 3 missing, keyframe 4 arrives: no need to wait.
        for p in packets(4, true, 100, 1150) {
            r.push(&p, t).unwrap();
        }
        assert_eq!(drain(&mut r, t).iter().map(|f| f.frame).collect::<Vec<_>>(), [4]);
        let mut other = Vec::new();
        packetize(10, 5, false, 0, &[1, 2, 3], 1150, |p| other = p.to_vec()).unwrap();
        assert_eq!(r.push(&other, t), Err(PushError::OtherStream));
        assert_eq!(r.push(&[1, 2, 3], t), Err(PushError::Malformed));
    }

    #[test]
    fn wraparound_and_restart() {
        let t = Instant::now();
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        let mut got = Vec::new();
        for f in [u32::MAX - 1, u32::MAX, 0, 1] {
            let mut out = Vec::new();
            packetize(9, f, f == u32::MAX - 1, 0, &[f as u8; 10], 1150, |p| out.push(p.to_vec())).unwrap();
            for p in out {
                r.push(&p, t).unwrap();
            }
            got.extend(drain(&mut r, t).into_iter().map(|c| c.frame));
        }
        assert_eq!(got, [u32::MAX - 1, u32::MAX, 0, 1]);
        // Sender restarts its counter far behind: accepted as a new stream.
        let mut r = Reassembler::new(9, ReassemblerConfig::default());
        for f in [5000u32, 5001] {
            for p in packets(f, f == 5000, 10, 1150) {
                r.push(&p, t).unwrap();
            }
        }
        assert_eq!(drain(&mut r, t).len(), 2);
        for p in packets(0, true, 10, 1150) {
            r.push(&p, t).unwrap();
        }
        assert_eq!(drain(&mut r, t).iter().map(|f| f.frame).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn random_loss_never_delivers_undecodable_frames() {
        // Property: every delivered frame is either a keyframe or the direct successor of the
        // previously delivered frame, under random loss, duplication and reordering.
        let t0 = Instant::now();
        for seed in 1..20u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut r = Reassembler::new(9, ReassemblerConfig { max_frames: 16, ..Default::default() });
            let mut wire: Vec<(u64, Vec<u8>)> = Vec::new();
            for f in 0..300u32 {
                let send_ms = f as u64 * 33;
                for p in packets(f, f % 30 == 0, 200 + rng.below(8000), 1150) {
                    if rng.below(100) < 3 {
                        continue; // 3% loss
                    }
                    let jitter = rng.below(60) as u64;
                    if rng.below(100) < 5 {
                        wire.push((send_ms + jitter + 10, p.clone()));
                    }
                    wire.push((send_ms + jitter, p));
                }
            }
            wire.sort_by_key(|w| w.0);
            let mut last: Option<u32> = None;
            let mut delivered = 0;
            for (at, p) in &wire {
                let now = t0 + Duration::from_millis(*at);
                let _ = r.push(p, now);
                while let Some(f) = r.pop(now) {
                    assert!(
                        f.keyframe || last.is_some_and(|l| l.wrapping_add(1) == f.frame),
                        "seed {seed}: {last:?} -> {}",
                        f.frame
                    );
                    assert_eq!(&f.data[..16], &frame_bytes(f.frame, 16)[..]);
                    last = Some(f.frame);
                    delivered += 1;
                }
            }
            assert!(delivered > 30, "seed {seed}: only {delivered} delivered");
            assert!(r.partial.len() <= 16);
        }
    }
}
