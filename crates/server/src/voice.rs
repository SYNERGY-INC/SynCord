//! Voice and video relay. The server never decodes audio or video: it checks
//! who sent a packet and forwards it to everyone else in the same voice channel,
//! over UDP when the listener's UDP path works and over their WebSocket otherwise.
//! Cameras go to everyone in the call; screen shares only to people watching them.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use rand::Rng;
use syncord_proto::voice::{
    Direction, Key, MAX_PACKET, Packet, ReplayWindow, Token, open, open_video, seal, seal_video,
};
use syncord_proto::{ChannelId, UserId, VideoKind, VoiceState};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A UDP path counts as working if we've heard from it this recently.
const UDP_FRESH: Duration = Duration::from_secs(15);
/// Audio packets allowed per second per person (50 is normal for 20 ms frames).
const MAX_PACKETS_PER_SEC: u32 = 75;
/// Video packets allowed per second per person (about 13 Mbit/s; a 1080p screen
/// share needs a few hundred a second, more for a moment at each keyframe).
const MAX_VIDEO_PACKETS_PER_SEC: u32 = 1500;

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
    /// Video counters seen from them.
    video_up: ReplayWindow,
    video_window_start: Instant,
    video_window_count: u32,
    /// Whose screen shares they're watching.
    watching: HashSet<UserId>,
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
        let joined_at = crate::db::now_ms();
        let state = VoiceState {
            user_id,
            channel_id,
            muted,
            deafened,
            ssrc,
            server_muted,
            server_deafened,
            joined_at,
            camera: false,
            screen: false,
        };
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
            video_up: ReplayWindow::default(),
            video_window_start: Instant::now(),
            video_window_count: 0,
            watching: HashSet::new(),
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
        // joined_at is when they arrived in this channel (it drives the channel's call timer).
        m.state.joined_at = crate::db::now_ms();
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

    /// Server mute and deafen don't apply in private calls: lift them (without
    /// forgetting them) for someone who just joined one.
    pub fn exempt(&self, user_id: UserId) -> Option<VoiceState> {
        let mut members = self.lock();
        let m = members.get_mut(&user_id)?;
        m.state.server_muted = false;
        m.state.server_deafened = false;
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

    /// Camera or screen share turned on or off. Turning a screen share off stops
    /// everyone watching it, so starting a new one doesn't send it to them unasked.
    pub fn set_video(&self, user_id: UserId, conn_id: u64, camera: bool, screen: bool) -> Option<VoiceState> {
        let mut members = self.lock();
        let m = members.get_mut(&user_id).filter(|m| m.conn_id == conn_id)?;
        m.state.camera = camera;
        m.state.screen = screen;
        let state = m.state.clone();
        if !screen {
            for m in members.values_mut() {
                m.watching.remove(&user_id);
            }
        }
        Some(state)
    }

    /// Start or stop watching someone's screen. True if `viewer` now watches a
    /// screen share in their own call.
    pub fn watch(&self, viewer: UserId, target: UserId, on: bool) -> bool {
        let mut members = self.lock();
        let shares = members.get(&target).filter(|t| t.state.screen).map(|t| t.state.channel_id);
        let Some(m) = members.get_mut(&viewer) else { return false };
        if on && shares == Some(m.state.channel_id) && viewer != target {
            m.watching.insert(target);
            true
        } else {
            m.watching.remove(&target);
            false
        }
    }

    /// Whether `from` may ask `to` for a keyframe of this video: they're in the same
    /// call, it's on, and (for screens) `from` is watching it.
    pub fn may_request_keyframe(&self, from: UserId, to: UserId, kind: VideoKind) -> bool {
        let members = self.lock();
        let (Some(a), Some(b)) = (members.get(&from), members.get(&to)) else { return false };
        a.state.channel_id == b.state.channel_id
            && match kind {
                VideoKind::Camera => b.state.camera,
                VideoKind::Screen => b.state.screen && a.watching.contains(&to),
            }
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
            Packet::VideoUp { token, counter, sealed } => {
                self.relay_video(token, counter, sealed, from);
                None
            }
            Packet::Ack | Packet::AudioDown { .. } | Packet::SealedDown { .. } | Packet::VideoDown { .. } => None,
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

impl Voice {
    /// Forward one video packet to whoever should see it, sealed with each one's key.
    fn relay_video(&self, token: Token, counter: u64, sealed: &[u8], from: Option<SocketAddr>) {
        let udp = self.udp.lock().unwrap().clone();
        let mut members = self.lock();
        let Some(sender) = members.values_mut().find(|m| m.token == token) else { return };
        // Video is always encrypted, so only apps that asked for a key can send it.
        let Some(key) = sender.key else { return };
        let Some((kind, data)) = open_video(&key, Direction::Up, counter, sealed) else { return };
        if !sender.video_up.accept(counter) {
            return;
        }
        let on = match kind {
            VideoKind::Camera => sender.state.camera,
            VideoKind::Screen => sender.state.screen,
        };
        if !on {
            return;
        }
        if let Some(addr) = from {
            sender.udp = Some((addr, Instant::now()));
        }
        if sender.video_window_start.elapsed() >= Duration::from_secs(1) {
            sender.video_window_start = Instant::now();
            sender.video_window_count = 0;
        }
        sender.video_window_count += 1;
        if sender.video_window_count > MAX_VIDEO_PACKETS_PER_SEC {
            return;
        }
        let (sender_id, channel_id, ssrc) = (sender.state.user_id, sender.state.channel_id, sender.state.ssrc);

        for m in members.values_mut() {
            if m.state.user_id == sender_id || m.state.channel_id != channel_id {
                continue;
            }
            let wanted = match kind {
                VideoKind::Camera => true,
                VideoKind::Screen => m.watching.contains(&sender_id),
            };
            let Some(key) = m.key.filter(|_| wanted) else { continue };
            let counter = m.down_counter;
            m.down_counter += 1;
            let sealed = seal_video(&key, Direction::Down, counter, kind, &data);
            let bytes = Bytes::from(Packet::VideoDown { ssrc, counter, sealed: &sealed }.encode());
            let fresh_udp = m.udp.filter(|(_, seen)| seen.elapsed() < UDP_FRESH).map(|(addr, _)| addr);
            match (fresh_udp, &udp) {
                (Some(addr), Some(sock)) => {
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
    fn cameras_go_to_everyone_screens_to_watchers() {
        use syncord_proto::voice::{open_video, seal_video};
        let v = Voice::default();
        let mut keys = Vec::new();
        let mut rxs = Vec::new();
        for user in 1..=3 {
            let (tx, rx) = mpsc::channel(64);
            let j = v.join(user, 10, user as u64, tx, false, true, true);
            keys.push((j.token, j.key.unwrap()));
            rxs.push(rx);
        }
        let (old_token, mut old_rx) = join(&v, 4, 10); // no key: gets no video
        let (alice_token, alice_key) = keys[0];
        let send = |counter: u64, kind| {
            let sealed = seal_video(&alice_key, Direction::Up, counter, kind, &[counter as u8; 40]);
            v.handle_packet(&Packet::VideoUp { token: alice_token, counter, sealed: &sealed }.encode(), None);
        };
        let got = |rx: &mut mpsc::Receiver<Bytes>, key: &Key| -> Option<(VideoKind, Vec<u8>)> {
            let b = rx.try_recv().ok()?;
            let Some(Packet::VideoDown { counter, sealed, .. }) = Packet::parse(&b) else { panic!("video") };
            open_video(key, Direction::Down, counter, sealed)
        };

        // Off: nothing goes out.
        send(0, VideoKind::Camera);
        assert!(rxs[1].try_recv().is_err());
        // Camera on: Bob and Carol (deafened) both see it, sealed with their own keys.
        v.set_video(1, 1, true, true).unwrap();
        send(1, VideoKind::Camera);
        assert_eq!(got(&mut rxs[1], &keys[1].1), Some((VideoKind::Camera, vec![1; 40])));
        assert_eq!(got(&mut rxs[2], &keys[2].1), Some((VideoKind::Camera, vec![1; 40])));
        assert!(old_rx.try_recv().is_err());
        assert!(rxs[0].try_recv().is_err(), "no echo");
        // Replays are dropped, late packets aren't.
        send(1, VideoKind::Camera);
        assert!(rxs[1].try_recv().is_err());
        send(3, VideoKind::Screen);
        send(2, VideoKind::Camera);
        assert_eq!(got(&mut rxs[1], &keys[1].1), Some((VideoKind::Camera, vec![2; 40])), "screen not watched");
        assert_eq!(got(&mut rxs[2], &keys[2].1), Some((VideoKind::Camera, vec![2; 40])));

        // Bob watches the screen; only he gets it.
        assert!(v.watch(2, 1, true));
        assert!(!v.watch(4, 2, true), "bob isn't sharing");
        assert!(v.may_request_keyframe(2, 1, VideoKind::Screen));
        assert!(!v.may_request_keyframe(3, 1, VideoKind::Screen));
        send(4, VideoKind::Screen);
        assert_eq!(got(&mut rxs[1], &keys[1].1), Some((VideoKind::Screen, vec![4; 40])));
        assert!(rxs[2].try_recv().is_err());
        // Stopping the share stops the watching.
        v.set_video(1, 1, true, false).unwrap();
        v.set_video(1, 1, true, true).unwrap();
        send(5, VideoKind::Screen);
        assert!(rxs[1].try_recv().is_err());
        // Old apps can't send video.
        v.handle_packet(&Packet::VideoUp { token: old_token, counter: 0, sealed: &[0; 40] }.encode(), None);
        assert!(rxs[0].try_recv().is_err());
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
