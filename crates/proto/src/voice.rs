//! Binary voice packet formats, shared by client and server.
//!
//! Audio is mono Opus at 48 kHz in 20 ms frames. The same packets travel over
//! UDP (preferred, lowest latency) or as binary WebSocket frames (fallback).
//!
//! ```text
//! client -> server  HELLO   [0x01][token: 16]
//! server -> client  ACK     [0x02]
//! client -> server  AUDIO   [0x03][token: 16][seq: u16 BE][opus...]
//! server -> client  AUDIO   [0x04][ssrc: u32 BE][seq: u16 BE][opus...]
//! client -> server  SEALED  [0x05][token: 16][counter: u64 BE][sealed(seq: u16 BE, opus...)]
//! server -> client  SEALED  [0x06][ssrc: u32 BE][counter: u64 BE][sealed(seq: u16 BE, opus...)]
//! ```
//!
//! Sealed packets are encrypted and authenticated with ChaCha20-Poly1305 using a
//! per-session key that the server hands out over the (encrypted) WebSocket.
//! The nonce is the direction (0 = up, 1 = down) and the sender's packet counter,
//! so a key never reuses a nonce. Plain AUDIO packets are for apps older than 0.5.

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_MS: u64 = 20;
pub const TOKEN_LEN: usize = 16;
/// Largest packet we'll send or accept.
pub const MAX_PACKET: usize = 1400;

pub type Token = [u8; TOKEN_LEN];
/// Per-session voice encryption key.
pub type Key = [u8; 32];
/// Bytes added by sealing (the authentication tag).
pub const SEAL_OVERHEAD: usize = 16;

const HELLO: u8 = 1;
const ACK: u8 = 2;
const AUDIO_UP: u8 = 3;
const AUDIO_DOWN: u8 = 4;
const SEALED_UP: u8 = 5;
const SEALED_DOWN: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    Hello { token: Token },
    Ack,
    AudioUp { token: Token, seq: u16, opus: &'a [u8] },
    AudioDown { ssrc: u32, seq: u16, opus: &'a [u8] },
    SealedUp { token: Token, counter: u64, sealed: &'a [u8] },
    SealedDown { ssrc: u32, counter: u64, sealed: &'a [u8] },
}

impl<'a> Packet<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() > MAX_PACKET {
            return None;
        }
        let (&kind, rest) = data.split_first()?;
        match kind {
            HELLO => Some(Packet::Hello { token: rest.get(..TOKEN_LEN)?.try_into().ok()? }),
            ACK => Some(Packet::Ack),
            AUDIO_UP => {
                let token = rest.get(..TOKEN_LEN)?.try_into().ok()?;
                let seq = u16::from_be_bytes(rest.get(TOKEN_LEN..TOKEN_LEN + 2)?.try_into().ok()?);
                let opus = rest.get(TOKEN_LEN + 2..)?;
                (!opus.is_empty()).then_some(Packet::AudioUp { token, seq, opus })
            }
            AUDIO_DOWN => {
                let ssrc = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
                let seq = u16::from_be_bytes(rest.get(4..6)?.try_into().ok()?);
                let opus = rest.get(6..)?;
                (!opus.is_empty()).then_some(Packet::AudioDown { ssrc, seq, opus })
            }
            SEALED_UP => {
                let token = rest.get(..TOKEN_LEN)?.try_into().ok()?;
                let counter = u64::from_be_bytes(rest.get(TOKEN_LEN..TOKEN_LEN + 8)?.try_into().ok()?);
                let sealed = rest.get(TOKEN_LEN + 8..)?;
                (sealed.len() > SEAL_OVERHEAD + 2).then_some(Packet::SealedUp { token, counter, sealed })
            }
            SEALED_DOWN => {
                let ssrc = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
                let counter = u64::from_be_bytes(rest.get(4..12)?.try_into().ok()?);
                let sealed = rest.get(12..)?;
                (sealed.len() > SEAL_OVERHEAD + 2).then_some(Packet::SealedDown { ssrc, counter, sealed })
            }
            _ => None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Packet::Hello { token } => [&[HELLO][..], &token].concat(),
            Packet::Ack => vec![ACK],
            Packet::AudioUp { token, seq, opus } => [&[AUDIO_UP][..], &token, &seq.to_be_bytes(), opus].concat(),
            Packet::AudioDown { ssrc, seq, opus } => {
                [&[AUDIO_DOWN][..], &ssrc.to_be_bytes(), &seq.to_be_bytes(), opus].concat()
            }
            Packet::SealedUp { token, counter, sealed } => {
                [&[SEALED_UP][..], &token, &counter.to_be_bytes(), sealed].concat()
            }
            Packet::SealedDown { ssrc, counter, sealed } => {
                [&[SEALED_DOWN][..], &ssrc.to_be_bytes(), &counter.to_be_bytes(), sealed].concat()
            }
        }
    }
}

/// Which way a sealed packet travels (part of the nonce, so the two directions never collide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up = 0,
    Down = 1,
}

fn nonce(direction: Direction, counter: u64) -> chacha20poly1305::Nonce {
    let mut n = [0u8; 12];
    n[0] = direction as u8;
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n.into()
}

/// Encrypt one audio frame (its sequence number and Opus data).
pub fn seal(key: &Key, direction: Direction, counter: u64, seq: u16, opus: &[u8]) -> Vec<u8> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(key.into());
    let plain = [&seq.to_be_bytes()[..], opus].concat();
    cipher.encrypt(&nonce(direction, counter), plain.as_slice()).expect("encrypting in memory can't fail")
}

/// Decrypt and check one audio frame; `None` if it was tampered with or uses another key.
pub fn open(key: &Key, direction: Direction, counter: u64, sealed: &[u8]) -> Option<(u16, Vec<u8>)> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(key.into());
    let plain = cipher.decrypt(&nonce(direction, counter), sealed).ok()?;
    let seq = u16::from_be_bytes(plain.get(..2)?.try_into().ok()?);
    let opus = plain.get(2..)?;
    (!opus.is_empty()).then(|| (seq, opus.to_vec()))
}

pub fn key_to_hex(key: &Key) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn key_from_hex(s: &str) -> Option<Key> {
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(std::str::from_utf8(&s[i * 2..i * 2 + 2]).ok()?, 16).ok()?;
    }
    Some(out)
}

pub fn token_to_hex(token: &Token) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_from_hex(s: &str) -> Option<Token> {
    if s.len() != TOKEN_LEN * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; TOKEN_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_roundtrip() {
        let token = [7u8; TOKEN_LEN];
        for p in [
            Packet::Hello { token },
            Packet::Ack,
            Packet::AudioUp { token, seq: 65535, opus: &[1, 2, 3] },
            Packet::AudioDown { ssrc: 0xDEADBEEF, seq: 9, opus: &[4, 5] },
        ] {
            assert_eq!(Packet::parse(&p.encode()), Some(p));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Packet::parse(&[]), None);
        assert_eq!(Packet::parse(&[9, 9, 9]), None);
        assert_eq!(Packet::parse(&[HELLO, 1, 2]), None);
        // Audio with no payload is invalid.
        assert_eq!(Packet::parse(&Packet::AudioDown { ssrc: 1, seq: 1, opus: &[] }.encode()), None);
        assert_eq!(Packet::parse(&vec![AUDIO_DOWN; MAX_PACKET + 1]), None);
    }

    #[test]
    fn sealed_packets() {
        let key: Key = [9; 32];
        let sealed = seal(&key, Direction::Up, 7, 300, &[1, 2, 3]);
        let p = Packet::SealedUp { token: [1; 16], counter: 7, sealed: &sealed };
        let wire = p.encode();
        let Some(Packet::SealedUp { counter, sealed: s, .. }) = Packet::parse(&wire) else { panic!() };
        assert_eq!(open(&key, Direction::Up, counter, s), Some((300, vec![1, 2, 3])));
        // Wrong key, wrong direction, wrong counter or a flipped bit: refused.
        assert_eq!(open(&[8; 32], Direction::Up, 7, &sealed), None);
        assert_eq!(open(&key, Direction::Down, 7, &sealed), None);
        assert_eq!(open(&key, Direction::Up, 8, &sealed), None);
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert_eq!(open(&key, Direction::Up, 7, &bad), None);
        // The audio itself isn't visible in the packet.
        let loud = vec![0xAB; 100];
        let s = seal(&key, Direction::Down, 1, 1, &loud);
        assert!(!s.windows(8).any(|w| w == &loud[..8]));
        assert_eq!(key_from_hex(&key_to_hex(&key)), Some(key));
        assert_eq!(key_from_hex("00"), None);
    }

    #[test]
    fn hex_tokens() {
        let token: Token = core::array::from_fn(|i| i as u8 * 17);
        let hex = token_to_hex(&token);
        assert_eq!(hex.len(), 32);
        assert_eq!(token_from_hex(&hex), Some(token));
        assert_eq!(token_from_hex("zz"), None);
    }
}
