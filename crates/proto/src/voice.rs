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
//! ```

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_MS: u64 = 20;
pub const TOKEN_LEN: usize = 16;
/// Largest packet we'll send or accept.
pub const MAX_PACKET: usize = 1400;

pub type Token = [u8; TOKEN_LEN];

const HELLO: u8 = 1;
const ACK: u8 = 2;
const AUDIO_UP: u8 = 3;
const AUDIO_DOWN: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    Hello { token: Token },
    Ack,
    AudioUp { token: Token, seq: u16, opus: &'a [u8] },
    AudioDown { ssrc: u32, seq: u16, opus: &'a [u8] },
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
        }
    }
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
    fn hex_tokens() {
        let token: Token = core::array::from_fn(|i| i as u8 * 17);
        let hex = token_to_hex(&token);
        assert_eq!(hex.len(), 32);
        assert_eq!(token_from_hex(&hex), Some(token));
        assert_eq!(token_from_hex("zz"), None);
    }
}
