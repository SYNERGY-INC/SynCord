//! End-to-end encryption support (added in 0.7). The apps do all the cryptography;
//! the server only stores public keys and passes encrypted messages and call key
//! exchanges along without being able to read them.

use serde::{Deserialize, Serialize};

use crate::UserId;

/// Someone's public identity keys. Every value is standard base64 (with padding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityKeys {
    pub user_id: UserId,
    /// X25519 public key for key agreement (32 bytes).
    pub agreement_key: String,
    /// Ed25519 public key for signatures (32 bytes).
    pub signing_key: String,
    /// The app's signature over the keys, if it makes one (at most 128 bytes; not checked by the server).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    /// When they were published (Unix ms).
    #[serde(default)]
    pub updated_at: i64,
}

/// An encrypted message body in a direct message or group. The server stores and relays it
/// as it is; such messages have empty `content`, and aren't searched or previewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPayload {
    /// Base64 ciphertext (at most [`MAX_CIPHERTEXT_LEN`] characters).
    pub ciphertext: String,
    /// Base64 nonce.
    pub nonce: String,
    /// Which of the sender's keys was used (app-defined, at most 128 characters).
    #[serde(default)]
    pub sender_key_id: String,
    /// The message key wrapped for each recipient (groups), at most [`MAX_WRAPPED_KEYS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<WrappedKey>,
}

/// A message key encrypted for one person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    pub user_id: UserId,
    /// Base64.
    pub key: String,
    /// Base64, if the wrapping needs its own nonce.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nonce: String,
}

pub const MAX_CIPHERTEXT_LEN: usize = 40_000;
/// Longest nonce, key id or wrapped key, in characters.
pub const MAX_E2EE_FIELD_LEN: usize = 256;
pub const MAX_WRAPPED_KEYS: usize = crate::social::MAX_GROUP_SIZE;
/// Largest key-exchange blob relayed between call participants, in characters.
pub const MAX_CALL_SIGNAL_LEN: usize = 16_000;

impl EncryptedPayload {
    /// Sizes and base64 are as expected (the contents are never looked at).
    pub fn is_well_formed(&self) -> bool {
        let short = |s: &str| s.len() <= MAX_E2EE_FIELD_LEN;
        !self.ciphertext.is_empty()
            && self.ciphertext.len() <= MAX_CIPHERTEXT_LEN
            && b64_decode(&self.ciphertext).is_some()
            && short(&self.nonce)
            && b64_decode(&self.nonce).is_some()
            && short(&self.sender_key_id)
            && self.recipients.len() <= MAX_WRAPPED_KEYS
            && self.recipients.iter().all(|r| {
                short(&r.key)
                    && short(&r.nonce)
                    && b64_decode(&r.key).is_some()
                    && (r.nonce.is_empty() || b64_decode(&r.nonce).is_some())
            })
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard base64 (padding optional); `None` if it isn't valid.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A base64 value that decodes to exactly 32 bytes (a public key).
pub fn is_key32(s: &str) -> bool {
    s.len() <= 64 && b64_decode(s).is_some_and(|k| k.len() == 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)), Some(data));
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(b64_decode("a"), None);
        assert_eq!(b64_decode("a*bc"), None);
        assert!(is_key32(&b64_encode(&[7; 32])));
        assert!(!is_key32(&b64_encode(&[7; 31])));
    }

    #[test]
    fn payload_checks() {
        let ok = EncryptedPayload {
            ciphertext: b64_encode(b"secret"),
            nonce: b64_encode(&[1; 12]),
            sender_key_id: "k1".into(),
            recipients: vec![WrappedKey { user_id: 2, key: b64_encode(&[2; 48]), nonce: String::new() }],
        };
        assert!(ok.is_well_formed());
        assert!(!EncryptedPayload { ciphertext: "not base64!".into(), ..ok.clone() }.is_well_formed());
        assert!(!EncryptedPayload { ciphertext: String::new(), ..ok.clone() }.is_well_formed());
        assert!(!EncryptedPayload { ciphertext: "A".repeat(MAX_CIPHERTEXT_LEN + 4), ..ok }.is_well_formed());
    }
}
