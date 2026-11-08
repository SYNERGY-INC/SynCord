//! Voice relay. The server never decodes audio: it checks who sent a packet
//! and forwards it to everyone else in the same voice channel, over UDP when
//! the listener's UDP path works and over their WebSocket otherwise.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use rand::Rng;
use syncord_proto::voice::{Direction, Key, MAX_PACKET, Packet, Token, open, seal};
use syncord_proto::{ChannelId, UserId, VoiceState};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A UDP path counts as working if we've heard from it this recently.
const UDP_FRESH: Duration = Duration::from_secs(15);
/// Audio packets allowed per second per person (50 is normal for 20 ms frames).
const MAX_PACKETS_PER_SEC: u32 = 75;

struct Member {
    state: VoiceState,
    token: Token,
    /// Set when this person's audio is encrypted.
    key: Option<Key>,
    /// Next counter for packets sealed for this person.
    down_counter: u64,
    /// Highest counter seen from them (old packets are dropped, so they can't be replayed).
    up_counter: Option<u64>,
    /// Which WebSocket connection owns this voice session.
    conn_id: u64,
    /// Binary frames queued for that WebSocket.
    ws: mpsc::Sender<Bytes>,
    udp: Option<(SocketAddr, Instant)>,
    window_start: Instant,
    window_count: u32,
}

#[derive(Default)]
pub struct Voice {
    members: Mutex<HashMap<UserId, Member>>,
    /// Server mute/deafen per person, remembered across leaving and rejoining.
    moderation: Mutex<HashMap<UserId, (bool, bool)>>,
    next_ssrc: AtomicU32,
    udp: Mutex<Option<Arc<UdpSocket>>>,
}

/// Returned by [`Voice::join`].
pub struct Joined {
    pub state: VoiceState,
    pub token: Token,
    pub key: Option<Key>,
    /// Set if the user was already in voice on another connection and got moved.
    pub replaced_conn: Option<u64>,
}

impl Voice {
    fn lock(&self) -> MutexGuard<'_, HashMap<UserId, Member>> {
        self.members.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_udp(&self, socket: Arc<UdpSocket>) {
        *self.udp.lock().unwrap() = Some(socket);
    }

    /// The voice channel someone is in, if any.
    pub fn channel_of(&self, user_id: UserId) -> Option<ChannelId> {
        self.lock().get(&user_id).map(|m| m.state.channel_id)
    }

    pub fn states(&self) -> Vec<VoiceState> {
        self.lock().values().map(|m| m.state.clone()).collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn join(
        &self,
        user_id: UserId,
        channel_id: ChannelId,
        conn_id: u64,
        ws: mpsc::Sender<Bytes>,
        muted: bool,
        deafened: bool,
        encrypted: bool,
    ) -> Joined {
        let token: Token = rand::rng().random();
        let key: Option<Key> = encrypted.then(|| rand::rng().random());
        let ssrc = self.next_ssrc.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let (server_muted, server_deafened) =
            self.moderation.lock().unwrap_or_else(|e| e.into_inner()).get(&user_id).copied().unwrap_or_default();
        let state = VoiceState { user_id, channel_id, muted, deafened, ssrc, server_muted, server_deafened };
        let member = Member {
            state: state.clone(),
            token,
            key,
            down_counter: 0,
            up_counter: None,
            conn_id,
            ws,
            udp: None,
            window_start: Instant::now(),
            window_count: 0,
        };
        let previous = self.lock().insert(user_id, member);
        Joined { state, token, key, replaced_conn: previous.map(|m| m.conn_id).filter(|c| *c != conn_id) }
    }

    /// Move someone already in voice to another channel. Their audio follows straight away
    /// (each listener's audio is sealed with that listener's own key).
    pub fn move_member(&self, user_id: UserId, channel_id: ChannelId) -> Option<VoiceState> {
        let mut members = self.lock();
        let m = members.get_mut(&user_id).filter(|m| m.state.channel_id != channel_id)?;
        m.state.channel_id = channel_id;
        Some(m.state.clone())
    }

    /// Set someone's server mute/deafen. Returns their new state if they're in voice now.
    pub fn moderate(&self, user_id: UserId, muted: bool, deafened: bool) -> Option<VoiceState> {
        {
            let mut moderation = self.moderation.lock().unwrap_or_else(|e| e.into_inner());
            if muted || deafened {
                moderation.insert(user_id, (muted, deafened));
            } else {
                moderation.remove(&user_id);
            }
        }
        let mut members = self.lock();
        let m = members.get_mut(&user_id)?;
        m.state.server_muted = muted;
        m.state.server_deafened = deafened;
        Some(m.state.clone())
    }

    /// Remove the user if `conn_id` owns their voice session. Returns true if they left.
    pub fn leave(&self, user_id: UserId, conn_id: u64) -> bool {
        let mut members = self.lock();
        if members.get(&user_id).is_some_and(|m| m.conn_id == conn_id) {
            members.remove(&user_id);
            true
        } else {
            false
        }
    }

    pub fn set_state(&self, user_id: UserId, conn_id: u64, muted: bool, deafened: bool) -> Option<VoiceState> {
        let mut members = self.lock();
        let m = members.get_mut(&user_id).filter(|m| m.conn_id == conn_id)?;
        m.state.muted = muted;
        m.state.deafened = deafened;
        Some(m.state.clone())
    }

    /// Remove everyone in a channel that was deleted; returns who was removed.
    pub fn clear_channel(&self, channel_id: ChannelId) -> Vec<UserId> {
        let mut members = self.lock();
        let gone: Vec<UserId> =
            members.values().filter(|m| m.state.channel_id == channel_id).map(|m| m.state.user_id).collect();
        for id in &gone {
            members.remove(id);
        }
        gone
    }

    /// Handle one packet from a client. `from` is set for UDP packets.
    /// Returns a reply to send back over UDP (the hello acknowledgement).
    pub fn handle_packet(&self, data: &[u8], from: Option<SocketAddr>) -> Option<Vec<u8>> {
        match Packet::parse(data)? {
            Packet::Hello { token } => {
                let addr = from?;
                let mut members = self.lock();
                let m = members.values_mut().find(|m| m.token == token)?;
                m.udp = Some((addr, Instant::now()));
                Some(Packet::Ack.encode())
            }
            Packet::AudioUp { token, seq, opus } => {
                self.relay(token, None, seq, opus, from);
                None
            }
            Packet::SealedUp { token, counter, sealed } => {
                let key = self.lock().values().find(|m| m.token == token).and_then(|m| m.key)?;
                let (seq, opus) = open(&key, Direction::Up, counter, sealed)?;
                self.relay(token, Some(counter), seq, &opus, from);
                None
            }
            Packet::Ack | Packet::AudioDown { .. } | Packet::SealedDown { .. } => None,
        }
    }

    /// Forward audio to the others in the channel. `counter` is set for sealed packets.
    fn relay(&self, token: Token, counter: Option<u64>, seq: u16, opus: &[u8], from: Option<SocketAddr>) {
        let udp = self.udp.lock().unwrap().clone();
        let mut members = self.lock();
        let Some(sender) = members.values_mut().find(|m| m.token == token) else { return };
        match (sender.key.is_some(), counter) {
            // Someone who encrypts never sends plain audio: refuse it (it could be forged).
            (true, None) => return,
            (true, Some(c)) => {
                if sender.up_counter.is_some_and(|last| c <= last) {
                    return; // repeated or very late: drop
                }
                sender.up_counter = Some(c);
            }
            (false, _) => {}
        }

        if let Some(addr) = from {
            sender.udp = Some((addr, Instant::now()));
        }
        if sender.window_start.elapsed() >= Duration::from_secs(1) {
            sender.window_start = Instant::now();
            sender.window_count = 0;
        }
        sender.window_count += 1;
        let silenced = sender.state.muted || sender.state.deafened || sender.state.server_muted;
        if sender.window_count > MAX_PACKETS_PER_SEC || silenced {
            return;
        }

        let (sender_id, channel_id, ssrc) = (sender.state.user_id, sender.state.channel_id, sender.state.ssrc);
        let plain = Bytes::from(Packet::AudioDown { ssrc, seq, opus }.encode());

        for m in members.values_mut() {
            let not_listening = m.state.deafened || m.state.server_deafened;
            if m.state.user_id == sender_id || m.state.channel_id != channel_id || not_listening {
                continue;
            }
            // Each listener gets the audio sealed with their own key (or plain, for old apps).
            let bytes = match m.key {
                Some(key) => {
                    let counter = m.down_counter;
                    m.down_counter += 1;
                    let sealed = seal(&key, Direction::Down, counter, seq, opus);
                    Bytes::from(Packet::SealedDown { ssrc, counter, sealed: &sealed }.encode())
                }
                None => plain.clone(),
            };
            let fresh_udp = m.udp.filter(|(_, seen)| seen.elapsed() < UDP_FRESH).map(|(addr, _)| addr);
            match (fresh_udp, &udp) {
                (Some(addr), Some(sock)) => {
                    // Non-blocking: if the OS buffer is full, dropping a voice packet is fine.
                    if let Err(e) = sock.try_send_to(&bytes, addr) {
                        debug!("udp send to {addr} failed: {e}");
                    }
                }
                _ => {
                    let _ = m.ws.try_send(bytes);
                }
            }
        }
    }
}

/// Receive loop for the UDP voice socket.
pub async fn run_udp(voice: Arc<Voice>, socket: Arc<UdpSocket>) {
    let mut buf = vec![0u8; MAX_PACKET + 1];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, from)) => {
                if let Some(reply) = voice.handle_packet(&buf[..len], Some(from)) {
                    let _ = socket.try_send_to(&reply, from);
                }
            }
            Err(e) => {
                // On Windows, an ICMP "port unreachable" from a client that went away shows up here.
                warn!("udp receive error: {e}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(v: &Voice, user: UserId, channel: ChannelId) -> (Token, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(16);
        let j = v.join(user, channel, user as u64, tx, false, false, false);
        (j.token, rx)
    }

    #[test]
    fn moving_and_server_moderation() {
        let v = Voice::default();
        let (alice, _a) = join(&v, 1, 10);
        let (_bob, mut bob_rx) = join(&v, 2, 20);
        let speak = |seq| v.handle_packet(&Packet::AudioUp { token: alice, seq, opus: &[7] }.encode(), None);

        speak(1);
        assert!(bob_rx.try_recv().is_err(), "different channels");
        assert_eq!(v.move_member(2, 10).unwrap().channel_id, 10);
        assert!(v.move_member(2, 10).is_none(), "already there");
        speak(2);
        assert!(bob_rx.try_recv().is_ok(), "bob hears alice after being moved");

        assert!(v.moderate(1, true, false).unwrap().server_muted);
        speak(3);
        assert!(bob_rx.try_recv().is_err(), "server-muted alice is silent");
        v.moderate(1, false, false);
        v.moderate(2, false, true);
        speak(4);
        assert!(bob_rx.try_recv().is_err(), "server-deafened bob hears nothing");

        // Server deafen sticks when bob leaves and rejoins.
        assert!(v.leave(2, 2));
        let (_bob, _rx) = join(&v, 2, 10);
        assert!(v.states().iter().find(|s| s.user_id == 2).unwrap().server_deafened);
        assert!(v.moderate(3, true, true).is_none(), "not in voice, but remembered");
    }

    #[test]
    fn encrypted_and_plain_members_hear_each_other() {
        let v = Voice::default();
        let (tx, mut alice_rx) = mpsc::channel(16);
        let alice = v.join(1, 10, 1, tx, false, false, true);
        let alice_key = alice.key.unwrap();
        let (tx, mut bob_rx) = mpsc::channel(16);
        let bob = v.join(2, 10, 2, tx, false, false, true);
        let bob_key = bob.key.unwrap();
        assert_ne!(alice_key, bob_key);
        let (old_token, mut old_rx) = join(&v, 3, 10); // an app older than 0.5

        // Alice (encrypted) speaks: Bob gets it sealed with his key, the old app gets it plain.
        let alice_sealed = seal(&alice_key, Direction::Up, 0, 42, &[7, 7]);
        let first = Packet::SealedUp { token: alice.token, counter: 0, sealed: &alice_sealed }.encode();
        v.handle_packet(&first, None);
        let got = bob_rx.try_recv().unwrap();
        let Some(Packet::SealedDown { counter, sealed, .. }) = Packet::parse(&got) else { panic!("sealed for bob") };
        assert_eq!(open(&bob_key, Direction::Down, counter, sealed), Some((42, vec![7, 7])));
        assert_eq!(open(&alice_key, Direction::Down, counter, sealed), None, "only bob's key opens it");
        let got = old_rx.try_recv().unwrap();
        assert!(matches!(Packet::parse(&got), Some(Packet::AudioDown { seq: 42, .. })));

        // Replays and plain packets with Alice's token are refused.
        v.handle_packet(&first, None);
        v.handle_packet(&Packet::AudioUp { token: alice.token, seq: 1, opus: &[1] }.encode(), None);
        assert!(bob_rx.try_recv().is_err());
        // Garbage sealed with the wrong key goes nowhere.
        let forged = seal(&[0; 32], Direction::Up, 5, 1, &[1]);
        v.handle_packet(&Packet::SealedUp { token: alice.token, counter: 5, sealed: &forged }.encode(), None);
        assert!(bob_rx.try_recv().is_err());

        // The old app speaks plain: Alice gets it sealed.
        v.handle_packet(&Packet::AudioUp { token: old_token, seq: 3, opus: &[5] }.encode(), None);
        let got = alice_rx.try_recv().unwrap();
        let Some(Packet::SealedDown { counter, sealed, .. }) = Packet::parse(&got) else { panic!() };
        assert_eq!(open(&alice_key, Direction::Down, counter, sealed), Some((3, vec![5])));
    }

    #[test]
    fn relays_within_channel_only() {
        let v = Voice::default();
        let (alice, mut alice_rx) = join(&v, 1, 10);
        let (_bob, mut bob_rx) = join(&v, 2, 10);
        let (_carol, mut carol_rx) = join(&v, 3, 99);

        let pkt = Packet::AudioUp { token: alice, seq: 5, opus: &[1, 2, 3] }.encode();
        v.handle_packet(&pkt, None);

        let got = bob_rx.try_recv().expect("bob should hear alice");
        let Some(Packet::AudioDown { seq: 5, opus: [1, 2, 3], .. }) = Packet::parse(&got) else { panic!() };
        assert!(alice_rx.try_recv().is_err(), "no echo back to the sender");
        assert!(carol_rx.try_recv().is_err(), "other channels don't hear it");
    }

    #[test]
    fn muted_and_deafened() {
        let v = Voice::default();
        let (alice, _a) = join(&v, 1, 10);
        let (_bob, mut bob_rx) = join(&v, 2, 10);
        let pkt = Packet::AudioUp { token: alice, seq: 1, opus: &[9] }.encode();

        v.set_state(1, 1, true, false);
        v.handle_packet(&pkt, None);
        assert!(bob_rx.try_recv().is_err(), "muted senders are dropped");

        v.set_state(1, 1, false, false);
        v.set_state(2, 2, false, true);
        v.handle_packet(&pkt, None);
        assert!(bob_rx.try_recv().is_err(), "deafened listeners get nothing");
    }

    #[test]
    fn bad_tokens_and_ownership() {
        let v = Voice::default();
        let (_alice, _a) = join(&v, 1, 10);
        let (_bob, mut bob_rx) = join(&v, 2, 10);
        let pkt = Packet::AudioUp { token: [0; 16], seq: 1, opus: &[9] }.encode();
        v.handle_packet(&pkt, None);
        assert!(bob_rx.try_recv().is_err());

        // A different connection can't remove someone else's voice session.
        assert!(!v.leave(1, 999));
        assert!(v.leave(1, 1));
        assert_eq!(v.states().len(), 1);
    }

    #[test]
    fn hello_needs_valid_token_and_address() {
        let v = Voice::default();
        let (alice, _a) = join(&v, 1, 10);
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert!(v.handle_packet(&Packet::Hello { token: alice }.encode(), None).is_none());
        assert!(v.handle_packet(&Packet::Hello { token: [1; 16] }.encode(), Some(addr)).is_none());
        assert_eq!(v.handle_packet(&Packet::Hello { token: alice }.encode(), Some(addr)), Some(vec![2]));
    }

    #[test]
    fn rate_limited() {
        let v = Voice::default();
        let (alice, _a) = join(&v, 1, 10);
        let (tx, mut bob_rx) = mpsc::channel(1000);
        v.join(2, 10, 2, tx, false, false, false);
        for seq in 0..200u16 {
            v.handle_packet(&Packet::AudioUp { token: alice, seq, opus: &[1] }.encode(), None);
        }
        let mut n = 0;
        while bob_rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, MAX_PACKETS_PER_SEC);
    }
}
