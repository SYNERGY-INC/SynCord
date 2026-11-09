//! End-to-end encryption of direct messages, group messages and calls.
//!
//! Each install has, per server, an identity: an X25519 key for agreeing on secrets and an
//! Ed25519 key that signs it. The public halves are published through the server's key
//! directory; the secret halves never leave this computer.
//!
//! A message gets a fresh random key K. The text is sealed with K (XChaCha20-Poly1305), and
//! K is sealed once per recipient (yourself included) with a key derived from X25519 between
//! the sender's identity and the recipient's. Only the people in the conversation can open
//! it; the server stores and relays bytes it can't read. Files aren't encrypted this way.
//!
//! Calls: everyone in a direct-message or group call makes a random key for what they send,
//! and hands it to each of the others sealed the same way (through the server's call signal
//! relay). Audio and video are sealed with it before the server's own transport encryption.

use std::collections::HashMap;
use std::sync::Mutex;

use celerity_proto::UserId;
use celerity_proto::e2ee::{EncryptedPayload, IdentityKeys, WrappedKey, b64_decode, b64_encode};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, XChaCha20Poly1305};
use ed25519_dalek::{Signer, Verifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// What's kept in settings (base64 secrets), per server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredIdentity {
    pub agreement: String,
    pub signing: String,
}

pub struct Identity {
    agreement: StaticSecret,
    signing: ed25519_dalek::SigningKey,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the system's random number generator");
    b
}

fn key32(b64: &str) -> Option<[u8; 32]> {
    b64_decode(b64)?.try_into().ok()
}

impl Identity {
    pub fn generate() -> Identity {
        Identity {
            agreement: StaticSecret::from(random::<32>()),
            signing: ed25519_dalek::SigningKey::from_bytes(&random::<32>()),
        }
    }

    pub fn from_stored(s: &StoredIdentity) -> Option<Identity> {
        Some(Identity {
            agreement: StaticSecret::from(key32(&s.agreement)?),
            signing: ed25519_dalek::SigningKey::from_bytes(&key32(&s.signing)?),
        })
    }

    pub fn to_stored(&self) -> StoredIdentity {
        StoredIdentity {
            agreement: b64_encode(self.agreement.as_bytes()),
            signing: b64_encode(&self.signing.to_bytes()),
        }
    }

    /// The public agreement key, as published (base64).
    pub fn public_key(&self) -> String {
        b64_encode(PublicKey::from(&self.agreement).as_bytes())
    }

    /// What to publish: the agreement key, the signing key, and the agreement key signed.
    pub fn published(&self) -> (String, String, String) {
        let agreement = PublicKey::from(&self.agreement);
        let signature = self.signing.sign(agreement.as_bytes());
        (
            b64_encode(agreement.as_bytes()),
            b64_encode(self.signing.verifying_key().as_bytes()),
            b64_encode(&signature.to_bytes()),
        )
    }

    /// A key only the holder of `their` secret and we can work out, for this purpose and
    /// these two public keys in this order (sender first).
    fn shared(&self, their: &[u8; 32], purpose: &[u8], sender: &[u8; 32], recipient: &[u8; 32]) -> [u8; 32] {
        let secret = self.agreement.diffie_hellman(&PublicKey::from(*their));
        let mut h = Sha256::new();
        h.update(b"celerity-e2ee-v1/");
        h.update(purpose);
        h.update(secret.as_bytes());
        h.update(sender);
        h.update(recipient);
        h.finalize().into()
    }
}

/// Someone's published keys hold together (the agreement key is signed by their signing key).
pub fn keys_are_signed(k: &IdentityKeys) -> bool {
    let (Some(agreement), Some(signing)) = (key32(&k.agreement_key), key32(&k.signing_key)) else { return false };
    let Ok(verifying) = ed25519_dalek::VerifyingKey::from_bytes(&signing) else { return false };
    let Some(sig) = b64_decode(&k.signature).and_then(|b| <[u8; 64]>::try_from(b).ok()) else { return false };
    verifying.verify(&agreement, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// What's inside an encrypted message.
#[derive(Serialize, Deserialize)]
struct Body {
    content: String,
}

/// Seal `content` for `recipients` (user id, their published agreement key), which should
/// include yourself so this device can read what it sent.
pub fn encrypt(me: &Identity, content: &str, recipients: &[(UserId, String)]) -> Option<EncryptedPayload> {
    let my_pub = PublicKey::from(&me.agreement).to_bytes();
    let message_key = random::<32>();
    let nonce = random::<24>();
    let body = serde_json::to_vec(&Body { content: content.to_string() }).ok()?;
    let ciphertext = XChaCha20Poly1305::new((&message_key).into()).encrypt((&nonce).into(), body.as_slice()).ok()?;
    let mut wrapped = Vec::new();
    for (user_id, their) in recipients {
        let their = key32(their)?;
        let wrap_key = me.shared(&their, b"message-key", &my_pub, &their);
        let wrap_nonce = random::<24>();
        // The message's own nonce is bound in, so a key can't be moved to another message.
        let sealed = XChaCha20Poly1305::new((&wrap_key).into())
            .encrypt((&wrap_nonce).into(), Payload { msg: &message_key, aad: &nonce })
            .ok()?;
        wrapped.push(WrappedKey { user_id: *user_id, key: b64_encode(&sealed), nonce: b64_encode(&wrap_nonce) });
    }
    Some(EncryptedPayload {
        ciphertext: b64_encode(&ciphertext),
        nonce: b64_encode(&nonce),
        sender_key_id: b64_encode(&my_pub),
        recipients: wrapped,
    })
}

/// Open a message sent to `me_id`. Also returns the sender's key it was sealed with (to check
/// against the author's published key).
pub fn decrypt(me: &Identity, me_id: UserId, p: &EncryptedPayload) -> Option<(String, String)> {
    let sender = key32(&p.sender_key_id)?;
    let my_pub = PublicKey::from(&me.agreement).to_bytes();
    let nonce: [u8; 24] = b64_decode(&p.nonce)?.try_into().ok()?;
    let mine = p.recipients.iter().find(|w| w.user_id == me_id)?;
    let wrap_nonce: [u8; 24] = b64_decode(&mine.nonce)?.try_into().ok()?;
    let wrap_key = me.shared(&sender, b"message-key", &sender, &my_pub);
    let message_key = XChaCha20Poly1305::new((&wrap_key).into())
        .decrypt((&wrap_nonce).into(), Payload { msg: &b64_decode(&mine.key)?, aad: &nonce })
        .ok()?;
    let message_key: [u8; 32] = message_key.try_into().ok()?;
    let body = XChaCha20Poly1305::new((&message_key).into())
        .decrypt((&nonce).into(), b64_decode(&p.ciphertext)?.as_slice())
        .ok()?;
    let body: Body = serde_json::from_slice(&body).ok()?;
    Some((body.content, p.sender_key_id.clone()))
}

// ---- Calls

/// A call key for someone else, sealed for them (sent as the call signal's data).
#[derive(Serialize, Deserialize)]
struct CallKeyOffer {
    v: u8,
    /// The sender's agreement key (base64), so the recipient can work out the wrap key.
    from_key: String,
    nonce: String,
    key: String,
}

/// Seal my call key for one other person.
pub fn offer_call_key(me: &Identity, call_key: &[u8; 32], their: &str) -> Option<String> {
    let my_pub = PublicKey::from(&me.agreement).to_bytes();
    let their = key32(their)?;
    let wrap_key = me.shared(&their, b"call-key", &my_pub, &their);
    let nonce = random::<24>();
    let sealed = XChaCha20Poly1305::new((&wrap_key).into()).encrypt((&nonce).into(), call_key.as_slice()).ok()?;
    serde_json::to_string(&CallKeyOffer {
        v: 1,
        from_key: b64_encode(&my_pub),
        nonce: b64_encode(&nonce),
        key: b64_encode(&sealed),
    })
    .ok()
}

/// Open a call key someone sent; returns it with the key it was sealed with.
pub fn accept_call_key(me: &Identity, data: &str) -> Option<([u8; 32], String)> {
    let offer: CallKeyOffer = serde_json::from_str(data).ok()?;
    let sender = key32(&offer.from_key)?;
    let my_pub = PublicKey::from(&me.agreement).to_bytes();
    let wrap_key = me.shared(&sender, b"call-key", &sender, &my_pub);
    let nonce: [u8; 24] = b64_decode(&offer.nonce)?.try_into().ok()?;
    let key =
        XChaCha20Poly1305::new((&wrap_key).into()).decrypt((&nonce).into(), b64_decode(&offer.key)?.as_slice()).ok()?;
    Some((key.try_into().ok()?, offer.from_key))
}

pub fn new_call_key() -> [u8; 32] {
    random::<32>()
}

/// Keys for the call you're in, used from the audio and video threads: yours (with a packet
/// counter for nonces) and everyone else's, by their ssrc.
#[derive(Default)]
struct CallKeys {
    mine: Option<([u8; 32], u64)>,
    theirs: HashMap<u32, [u8; 32]>,
}

static CALL: Mutex<Option<CallKeys>> = Mutex::new(None);

/// Start (or stop, with `None`) sealing this call's audio and video end to end.
pub fn set_call_key(key: Option<[u8; 32]>) {
    *CALL.lock().unwrap() = key.map(|k| CallKeys { mine: Some((k, 0)), theirs: HashMap::new() });
}

pub fn add_peer_key(ssrc: u32, key: [u8; 32]) {
    if let Some(c) = CALL.lock().unwrap().as_mut() {
        c.theirs.insert(ssrc, key);
    }
}

pub fn call_encrypted() -> bool {
    CALL.lock().unwrap().is_some()
}

/// Seal one packet's payload with my call key: `[counter u64][ciphertext+tag]`. Unchanged
/// when the call isn't end-to-end encrypted.
pub fn seal_media(payload: &[u8]) -> Vec<u8> {
    let mut call = CALL.lock().unwrap();
    let Some((key, counter)) = call.as_mut().and_then(|c| c.mine.as_mut()) else { return payload.to_vec() };
    *counter += 1;
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&counter.to_be_bytes());
    let Ok(sealed) = ChaCha20Poly1305::new((&*key).into()).encrypt((&nonce).into(), payload) else { return Vec::new() };
    let mut out = Vec::with_capacity(8 + sealed.len());
    out.extend_from_slice(&counter.to_be_bytes());
    out.extend_from_slice(&sealed);
    out
}

/// Open a packet from `ssrc`. `None` if it can't be (no key from them yet, or tampered with).
pub fn open_media(ssrc: u32, data: &[u8]) -> Option<Vec<u8>> {
    let call = CALL.lock().unwrap();
    let Some(call) = call.as_ref() else { return Some(data.to_vec()) };
    let key = call.theirs.get(&ssrc)?;
    if data.len() < 8 + 16 {
        return None;
    }
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&data[..8]);
    ChaCha20Poly1305::new(key.into()).decrypt((&nonce).into(), &data[8..]).ok()
}

// ---- The app's side

use celerity_proto::{ChannelId, ClientMsg, Message as ChatMessage, MessageId, ServerMsg};

use crate::app::App;

/// Shown instead of a message this device can't open.
pub const UNREADABLE: &str = "🔒 This message is end-to-end encrypted for another device, so it can't be shown here.";

impl App {
    /// This install's identity on the open server (made and saved the first time).
    pub(crate) fn identity(&mut self) -> Identity {
        let key = crate::net::server_key(&self.settings.server);
        if let Some(id) = self.settings.identities.get(&key).and_then(Identity::from_stored) {
            return id;
        }
        let id = Identity::generate();
        self.settings.identities.insert(key, id.to_stored());
        self.settings.save();
        id
    }

    /// After signing in: remember who has keys, and publish ours if the server doesn't
    /// have it yet (or has another device's).
    pub(crate) fn setup_e2ee(&mut self, keys: Vec<IdentityKeys>, supported: bool) {
        let me_pub = self.identity().public_key();
        let Some(s) = &mut self.session else { return };
        s.e2ee = supported;
        s.keys = keys.into_iter().filter(keys_are_signed).map(|k| (k.user_id, k)).collect();
        if !supported || s.keys.get(&s.me).is_some_and(|k| k.agreement_key == me_pub) {
            return;
        }
        let (agreement_key, signing_key, signature) = self.identity().published();
        self.send(ClientMsg::PublishIdentityKeys { agreement_key, signing_key, signature });
    }

    pub(crate) fn on_identity_keys(&mut self, keys: Vec<IdentityKeys>) {
        let Some(s) = &mut self.session else { return };
        for k in keys.into_iter().filter(keys_are_signed) {
            s.keys.insert(k.user_id, k);
        }
    }

    /// Everyone in this conversation (you included) with their keys, if it's a direct
    /// message where everyone has published one.
    fn recipients(&self, channel: ChannelId) -> Option<Vec<(UserId, String)>> {
        let s = self.session.as_ref()?;
        if !s.e2ee {
            return None;
        }
        let other = s.direct(channel)?.user_id;
        [s.me, other].iter().map(|u| s.keys.get(u).map(|k| (*u, k.agreement_key.clone()))).collect()
    }

    /// The conversation is end-to-end encrypted (for the lock in its header).
    pub(crate) fn is_encrypted(&self, channel: ChannelId) -> bool {
        self.recipients(channel).is_some()
    }

    /// Send a message end to end encrypted if the conversation can be. Returns false if not
    /// (so it's sent as usual).
    pub(crate) fn send_encrypted(
        &mut self,
        channel_id: ChannelId,
        content: &str,
        attachments: &[String],
        reply_to: Option<MessageId>,
    ) -> bool {
        let Some(to) = self.recipients(channel_id) else { return false };
        let Some(payload) = encrypt(&self.identity(), content, &to) else { return false };
        self.send(ClientMsg::SendEncrypted { channel_id, payload, attachments: attachments.to_vec(), reply_to });
        true
    }

    /// Editing an encrypted message keeps it encrypted.
    pub(crate) fn edit_encrypted(&mut self, message_id: MessageId, content: &str) -> bool {
        let channel = self
            .session
            .as_ref()
            .and_then(|s| {
                s.chans.iter().find(|(_, st)| st.messages.iter().any(|m| m.id == message_id && m.encrypted.is_some()))
            })
            .map(|(c, _)| *c);
        let Some(to) = channel.and_then(|c| self.recipients(c)) else { return false };
        let Some(payload) = encrypt(&self.identity(), content, &to) else { return false };
        self.send(ClientMsg::EditEncrypted { message_id, payload });
        true
    }

    /// Open the encrypted messages in something from the server, in place.
    pub(crate) fn decrypt_incoming(&mut self, msg: &mut ServerMsg) {
        let messages: Vec<&mut ChatMessage> = match msg {
            ServerMsg::MessageCreated(m) | ServerMsg::MessageUpdated(m) => vec![m],
            ServerMsg::History { messages, .. }
            | ServerMsg::Around { messages, .. }
            | ServerMsg::Pins { messages, .. } => messages.iter_mut().collect(),
            _ => return,
        };
        if messages.iter().all(|m| m.encrypted.is_none()) {
            return;
        }
        let me = self.identity();
        let Some(s) = &self.session else { return };
        for m in messages {
            let Some(p) = &m.encrypted else { continue };
            m.content = match decrypt(&me, s.me, p) {
                // Sealed with a key that isn't the author's: don't trust it.
                Some((text, key)) if s.keys.get(&m.author_id).is_none_or(|k| k.agreement_key == key) => {
                    m.mentions = celerity_proto::parse_mentions(&text).0;
                    text
                }
                Some(_) => "🔒 This message's encryption doesn't match its sender, so it isn't shown.".into(),
                None => UNREADABLE.into(),
            };
        }
    }

    // ---- Calls

    /// Joined a direct-message call: if everyone in it has keys, seal its audio and video
    /// end to end, and give the others our key.
    pub(crate) fn start_call_e2ee(&mut self, channel: ChannelId) {
        if self.recipients(channel).is_none() {
            set_call_key(None);
            return;
        }
        let key = new_call_key();
        set_call_key(Some(key));
        self.call_key = Some(key);
        self.call_key_sent.clear();
        // Keys others sent while we were still joining.
        let ssrcs: HashMap<UserId, u32> =
            self.call_session().map(|s| s.voice.values().map(|v| (v.user_id, v.ssrc)).collect()).unwrap_or_default();
        for (user, key) in std::mem::take(&mut self.pending_call_keys) {
            match ssrcs.get(&user) {
                Some(ssrc) => add_peer_key(*ssrc, key),
                None => {
                    self.pending_call_keys.insert(user, key);
                }
            }
        }
        self.offer_call_key_to_all();
    }

    /// Give our call key to everyone else in the call (they hand theirs back the same way).
    pub(crate) fn offer_call_key_to_all(&mut self) {
        let Some(key) = self.call_key else { return };
        let me = self.identity();
        let Some(s) = self.call_session() else { return };
        let Some(channel) = self.voice.as_ref().map(|v| v.channel_id).or(self.voice_joining) else { return };
        let others: Vec<(UserId, String)> = s
            .voice
            .values()
            .filter(|v| v.channel_id == channel && v.user_id != s.me)
            .filter_map(|v| s.keys.get(&v.user_id).map(|k| (v.user_id, k.agreement_key.clone())))
            .collect();
        for (user, their) in others {
            if let Some(data) = offer_call_key(&me, &key, &their) {
                self.send_call(ClientMsg::CallSignal { to: Some(user), data });
                self.call_key_sent.insert(user);
            }
        }
    }

    /// Someone in the call sent their key.
    pub(crate) fn on_call_signal(&mut self, from: UserId, data: &str) {
        // Still joining: keep it for when the call starts.
        if self.call_key.is_none() && self.voice_joining.is_none() && self.voice.is_none() {
            return;
        }
        let me = self.identity();
        let Some(s) = self.call_session() else { return };
        let Some((key, sealed_with)) = accept_call_key(&me, data) else { return };
        // Only from the key they published.
        if s.keys.get(&from).is_none_or(|k| k.agreement_key != sealed_with) {
            return;
        }
        match s.voice.get(&from).filter(|_| self.call_key.is_some()) {
            Some(v) => add_peer_key(v.ssrc, key),
            None => {
                self.pending_call_keys.insert(from, key);
            }
        }
        // They have ours too? (They may have joined before we could send it.)
        if self.call_key.is_some() && !self.call_key_sent.contains(&from) {
            self.offer_call_key_to_all();
        }
    }

    /// Someone joined (or rejoined) the call: give them our key, and use theirs if it came first.
    pub(crate) fn on_call_member(&mut self, user: UserId, ssrc: u32) {
        if self.call_key.is_none() {
            return;
        }
        if let Some(key) = self.pending_call_keys.remove(&user) {
            add_peer_key(ssrc, key);
        }
        self.offer_call_key_to_all();
    }

    pub(crate) fn stop_call_e2ee(&mut self) {
        self.call_key = None;
        self.call_key_sent.clear();
        self.pending_call_keys.clear();
        set_call_key(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(id: &Identity, user_id: UserId) -> IdentityKeys {
        let (agreement_key, signing_key, signature) = id.published();
        IdentityKeys { user_id, agreement_key, signing_key, signature, updated_at: 0 }
    }

    #[test]
    fn only_recipients_can_read() {
        let (alice, bob, eve) = (Identity::generate(), Identity::generate(), Identity::generate());
        let to = vec![(1, alice.public_key()), (2, bob.public_key())];
        let p = encrypt(&alice, "meet at 8", &to).unwrap();
        assert!(!p.ciphertext.contains("meet"));
        assert_eq!(decrypt(&bob, 2, &p).unwrap().0, "meet at 8");
        assert_eq!(decrypt(&alice, 1, &p).unwrap(), ("meet at 8".to_string(), alice.public_key()));
        // Not a recipient, or pretending to be one.
        assert!(decrypt(&eve, 3, &p).is_none());
        assert!(decrypt(&eve, 2, &p).is_none());
        // Tampering is caught.
        let mut bad = p.clone();
        bad.ciphertext = b64_encode(&[0u8; 40]);
        assert!(decrypt(&bob, 2, &bad).is_none());
    }

    #[test]
    fn identities_survive_saving_and_are_signed() {
        let id = Identity::generate();
        let back = Identity::from_stored(&id.to_stored()).unwrap();
        assert_eq!(back.public_key(), id.public_key());
        let mut keys = published(&id, 1);
        assert!(keys_are_signed(&keys));
        keys.agreement_key = Identity::generate().public_key();
        assert!(!keys_are_signed(&keys), "a swapped key no longer matches the signature");
    }

    #[test]
    fn call_keys_and_media() {
        let (alice, bob) = (Identity::generate(), Identity::generate());
        let key = new_call_key();
        let offer = offer_call_key(&alice, &key, &bob.public_key()).unwrap();
        let (got, from) = accept_call_key(&bob, &offer).unwrap();
        assert_eq!(got, key);
        assert_eq!(from, alice.public_key());
        assert!(accept_call_key(&alice, &offer).is_none(), "only the recipient can open it");

        set_call_key(Some(key));
        add_peer_key(7, key);
        let sealed = seal_media(b"opus frame");
        assert_ne!(&sealed[8..], b"opus frame");
        assert_eq!(open_media(7, &sealed).unwrap(), b"opus frame");
        assert!(open_media(8, &sealed).is_none(), "no key from that person");
        set_call_key(None);
        assert_eq!(seal_media(b"plain"), b"plain");
    }
}
