//! Per-speaker jitter buffer: absorbs uneven packet arrival and reordering so
//! playback stays smooth, at the cost of a little latency.

use std::collections::BTreeMap;

/// Frames (20 ms each) to collect before a speaker starts playing.
pub const PREBUFFER: usize = 3;
/// If more than this many frames pile up, skip ahead to cut latency.
pub const MAX_BUFFERED: usize = 12;

#[derive(Debug, PartialEq, Eq)]
pub enum Pop {
    /// Play this packet.
    Packet(Vec<u8>),
    /// A packet went missing mid-speech: the decoder should conceal the gap.
    Lost,
    /// Nothing to play (not talking, or still buffering).
    Idle,
}

#[derive(Default)]
pub struct JitterBuffer {
    /// Packets keyed by an "unwrapped" sequence number that never wraps around.
    packets: BTreeMap<u64, Vec<u8>>,
    /// Highest unwrapped sequence number seen, used to unwrap the next one.
    highest: Option<u64>,
    /// Next sequence number to play, while playing.
    next: Option<u64>,
}

/// Turn a 16-bit wrapping sequence number into a monotonic one, using the
/// closest value to the last one we saw.
fn unwrap_seq(last: u64, seq: u16) -> u64 {
    let base = last & !0xFFFF;
    let candidates = [base.wrapping_sub(0x10000), base, base + 0x10000];
    candidates.into_iter().map(|b| b | seq as u64).min_by_key(|c| c.abs_diff(last)).unwrap()
}

impl JitterBuffer {
    pub fn insert(&mut self, seq: u16, packet: Vec<u8>) {
        // Start far from zero so unwrapping never underflows.
        let ext = match self.highest {
            Some(h) => unwrap_seq(h, seq),
            None => (1 << 32) | seq as u64,
        };
        if self.next.is_some_and(|n| ext < n) {
            return; // arrived too late, already played past it
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        self.packets.insert(ext, packet);

        if self.packets.len() > MAX_BUFFERED {
            while self.packets.len() > PREBUFFER {
                self.packets.pop_first();
            }
            if self.next.is_some() {
                self.next = self.packets.keys().next().copied();
            }
        }
    }

    pub fn pop(&mut self) -> Pop {
        let next = match self.next {
            Some(n) => n,
            None => {
                if self.packets.len() < PREBUFFER {
                    return Pop::Idle;
                }
                *self.packets.keys().next().unwrap()
            }
        };

        if let Some(p) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            return Pop::Packet(p);
        }
        if self.packets.is_empty() {
            // Speaker went quiet: stop and re-buffer when they talk again.
            self.next = None;
            Pop::Idle
        } else {
            self.next = Some(next + 1);
            Pop::Lost
        }
    }

    #[cfg(test)]
    pub fn is_playing(&self) -> bool {
        self.next.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(n: u16) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    #[test]
    fn prebuffers_then_plays_in_order() {
        let mut j = JitterBuffer::default();
        j.insert(11, pkt(11));
        j.insert(10, pkt(10));
        assert_eq!(j.pop(), Pop::Idle, "still buffering");
        j.insert(12, pkt(12));
        assert_eq!(j.pop(), Pop::Packet(pkt(10)));
        assert_eq!(j.pop(), Pop::Packet(pkt(11)));
        assert_eq!(j.pop(), Pop::Packet(pkt(12)));
        assert_eq!(j.pop(), Pop::Idle, "talk spurt over");
        assert!(!j.is_playing());
    }

    #[test]
    fn conceals_gaps_and_drops_late_packets() {
        let mut j = JitterBuffer::default();
        for s in [1, 2, 4, 5] {
            j.insert(s, pkt(s));
        }
        assert_eq!(j.pop(), Pop::Packet(pkt(1)));
        assert_eq!(j.pop(), Pop::Packet(pkt(2)));
        assert_eq!(j.pop(), Pop::Lost);
        j.insert(3, pkt(3)); // too late now
        assert_eq!(j.pop(), Pop::Packet(pkt(4)));
        assert_eq!(j.pop(), Pop::Packet(pkt(5)));
        assert_eq!(j.pop(), Pop::Idle);
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut j = JitterBuffer::default();
        for s in [65534u16, 65535, 0, 1] {
            j.insert(s, pkt(s));
        }
        let order: Vec<Pop> = (0..4).map(|_| j.pop()).collect();
        assert_eq!(
            order,
            vec![Pop::Packet(pkt(65534)), Pop::Packet(pkt(65535)), Pop::Packet(pkt(0)), Pop::Packet(pkt(1))]
        );
    }

    #[test]
    fn skips_ahead_when_too_far_behind() {
        let mut j = JitterBuffer::default();
        for s in 0..3 {
            j.insert(s, pkt(s));
        }
        assert_eq!(j.pop(), Pop::Packet(pkt(0)));
        for s in 3..20 {
            j.insert(s, pkt(s));
        }
        // When the 13th frame piled up, the buffer was trimmed to the newest few (11, 12, 13).
        assert_eq!(j.pop(), Pop::Packet(pkt(11)));
        assert!(j.packets.len() <= MAX_BUFFERED);
    }

    #[test]
    fn unwrap() {
        assert_eq!(unwrap_seq(0x1_0000_FFFF, 0), 0x1_0001_0000);
        assert_eq!(unwrap_seq(0x1_0001_0000, 0xFFFF), 0x1_0000_FFFF);
        assert_eq!(unwrap_seq(0x1_0000_0005, 6), 0x1_0000_0006);
    }
}
