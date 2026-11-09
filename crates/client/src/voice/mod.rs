//! Voice chat engine.
//!
//! ```text
//! mic -> [cpal callback: mono, 48 kHz] -> capture thread: gain, noise suppression,
//!        voice gate, Opus -> UDP (or WebSocket fallback) -> server
//! server -> UDP thread / WebSocket -> Mixer: jitter buffer, Opus decode, mix
//!        -> [cpal output callback] -> speakers
//! ```
//!
//! Everything runs on its own threads so the UI never blocks on audio.

pub mod devices;
pub mod dsp;
pub mod echo;
pub mod jitter;
pub mod mixer;
pub mod ptt;

use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use celerity_proto::voice::{Direction, FRAME_SAMPLES, Key, Packet, Token, seal};

use crate::net::Connection;
use dsp::{CaptureProcessor, CaptureSettings};
use mixer::Mixer;

/// Sends one encoded audio frame (sequence number, Opus bytes) somewhere.
type PacketSender = Box<dyn FnMut(u16, &[u8]) + Send>;

/// Where the WebSocket task delivers voice packets that arrive over TCP.
pub type VoiceSink = Arc<Mutex<Option<Arc<Mixer>>>>;

/// User-adjustable audio settings.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    /// Voice gate threshold in dBFS; quieter sounds aren't sent.
    pub sensitivity_db: f32,
    pub noise_suppression: bool,
    /// Take what the speakers play back out of the microphone.
    pub echo_cancellation: bool,
    /// Only send while this key is held (see `ptt`), instead of when you speak.
    pub push_to_talk: Option<String>,
}

/// Packet counters for one voice session. They outlive the engine (which restarts when
/// you switch devices) because the server refuses counters it has already seen, and
/// an encryption key must never see the same counter twice.
#[derive(Debug, Default)]
pub struct Counters {
    pub audio: AtomicU64,
    pub video: AtomicU64,
}

/// How audio leaves this computer.
pub enum Link {
    /// Talk to the server: UDP if it gets through, otherwise the WebSocket.
    /// With `key`, audio is encrypted both ways (and video can be sent).
    Server { host: String, udp_port: u16, token: Token, key: Option<Key>, conn: Connection, counters: Arc<Counters> },
    /// Play your own microphone back to you (the mic test).
    Loopback,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub input_level_db: f32,
    pub transmitting: bool,
    pub udp: bool,
    /// ssrcs of people currently heard.
    pub speaking: Vec<u32>,
    pub problem: Option<String>,
    /// Round trip to the server over UDP, once measured.
    pub ping_ms: Option<u32>,
    /// Audio packets heard and missed since the call started.
    pub heard: u64,
    pub missed: u64,
}

struct Shared {
    stop: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    config: Mutex<VoiceConfig>,
    input_level: AtomicU32,
    transmitting: AtomicBool,
    udp_ok: Arc<AtomicBool>,
    /// Smoothed UDP round trip in ms (u32::MAX until measured).
    ping_ms: AtomicU32,
    problem: Mutex<Option<String>>,
    mixer: Arc<Mixer>,
}

pub struct VoiceEngine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Set while this engine carries the call's video (see `crate::live`).
    video_out: Option<u64>,
}

impl VoiceEngine {
    pub fn start(config: VoiceConfig, link: Link, muted: bool, deafened: bool) -> Result<Self, String> {
        let mixer = Arc::new(Mixer::default());
        mixer.set_volume(config.output_volume);
        // The mic test plays your own voice back on purpose: nothing to cancel there.
        let cancel_echo = !matches!(link, Link::Loopback);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            muted: AtomicBool::new(muted),
            deafened: AtomicBool::new(deafened),
            config: Mutex::new(config.clone()),
            input_level: AtomicU32::new((-100f32).to_bits()),
            transmitting: AtomicBool::new(false),
            udp_ok: Arc::new(AtomicBool::new(false)),
            ping_ms: AtomicU32::new(u32::MAX),
            problem: Mutex::new(None),
            mixer: mixer.clone(),
        });
        let mut threads = Vec::new();
        let mut video_out = None;

        // Audio devices live on their own thread: cpal streams can't move between threads on every OS.
        let (chunks_tx, chunks_rx) = mpsc::sync_channel::<Vec<f32>>(32);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        {
            let shared = shared.clone();
            threads.push(spawn("celerity-audio", move || {
                let input = devices::start_input(config.input_device.as_deref(), chunks_tx);
                let output = devices::start_output(config.output_device.as_deref(), shared.mixer.clone());
                let result = match (&input, &output) {
                    (Err(a), Err(b)) => Err(format!("{a}. {b}.")),
                    (Err(e), Ok(_)) => {
                        *shared.problem.lock().unwrap() = Some(format!("{e}. You can listen but not talk."));
                        Ok(())
                    }
                    (Ok(_), Err(e)) => {
                        *shared.problem.lock().unwrap() =
                            Some(format!("{e}. Others can hear you, but you can't hear them."));
                        Ok(())
                    }
                    (Ok(_), Ok(_)) => Ok(()),
                };
                let failed = result.is_err();
                let _ = ready_tx.send(result);
                if failed {
                    return;
                }
                while !shared.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(input);
                drop(output);
            }));
        }
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Relaxed);
                return Err("Audio devices didn't respond".into());
            }
        }

        let sender: PacketSender = match link {
            Link::Loopback => {
                let mixer = mixer.clone();
                Box::new(move |seq, opus| mixer.push(0, seq, opus))
            }
            Link::Server { host, udp_port, token, key, conn, counters } => {
                mixer.set_key(key);
                let udp = open_udp(&host, udp_port);
                if let Some(sock) = &udp {
                    let shared = shared.clone();
                    let sock = sock.try_clone().expect("clone udp socket");
                    threads.push(spawn("celerity-voice-udp", move || udp_loop(sock, token, shared)));
                }
                let udp = udp.and_then(|s| s.try_clone().ok()).map(Arc::new);
                // Video goes out the same way (same socket, so the server sees one address).
                if let Some(key) = key {
                    video_out = Some(crate::live::set_out(crate::live::VideoOut {
                        token,
                        key,
                        udp: udp.clone(),
                        udp_ok: shared.udp_ok.clone(),
                        conn: conn.clone(),
                        counters: counters.clone(),
                    }));
                }
                let shared = shared.clone();
                Box::new(move |seq, opus| {
                    let packet = match &key {
                        Some(key) => {
                            let counter = counters.audio.fetch_add(1, Ordering::Relaxed);
                            let sealed = seal(key, Direction::Up, counter, seq, opus);
                            Packet::SealedUp { token, counter, sealed: &sealed }.encode()
                        }
                        None => Packet::AudioUp { token, seq, opus }.encode(),
                    };
                    match &udp {
                        Some(sock) if shared.udp_ok.load(Ordering::Relaxed) => {
                            let _ = sock.send(&packet);
                        }
                        _ => conn.send_binary(packet),
                    }
                })
            }
        };

        let capture_shared = shared.clone();
        threads.push(spawn("celerity-capture", move || capture_loop(chunks_rx, capture_shared, sender, cancel_echo)));

        Ok(Self { shared, threads, video_out })
    }

    pub fn mixer(&self) -> Arc<Mixer> {
        self.shared.mixer.clone()
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
        self.apply_volume();
    }

    /// Update settings that can change on the fly (volumes, sensitivity, noise suppression).
    pub fn set_config(&self, config: &VoiceConfig) {
        *self.shared.config.lock().unwrap() = config.clone();
        self.apply_volume();
    }

    pub fn set_speaker_volume(&self, ssrc: u32, volume: f32) {
        self.shared.mixer.set_speaker_volume(ssrc, volume);
    }

    fn apply_volume(&self) {
        let volume = if self.shared.deafened.load(Ordering::Relaxed) {
            0.0
        } else {
            self.shared.config.lock().unwrap().output_volume
        };
        self.shared.mixer.set_volume(volume);
    }

    pub fn status(&self) -> Status {
        Status {
            input_level_db: f32::from_bits(self.shared.input_level.load(Ordering::Relaxed)),
            transmitting: self.shared.transmitting.load(Ordering::Relaxed),
            udp: self.shared.udp_ok.load(Ordering::Relaxed),
            speaking: self.shared.mixer.speaking(),
            problem: self.shared.problem.lock().unwrap().clone(),
            ping_ms: Some(self.shared.ping_ms.load(Ordering::Relaxed)).filter(|p| *p != u32::MAX),
            heard: self.shared.mixer.heard.load(Ordering::Relaxed),
            missed: self.shared.mixer.missed.load(Ordering::Relaxed),
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        if let Some(id) = self.video_out {
            crate::live::clear_out(id);
        }
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn audio thread")
}

fn capture_loop(chunks: mpsc::Receiver<Vec<f32>>, shared: Arc<Shared>, mut send: PacketSender, cancel_echo: bool) {
    let mut processor = match CaptureProcessor::new() {
        Ok(p) => p,
        Err(e) => {
            *shared.problem.lock().unwrap() = Some(format!("Voice encoder failed to start: {e}"));
            return;
        }
    };
    let mut buffer: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    // Made the first time it's wanted (it's switchable during a call).
    let mut echo: Option<echo::EchoCanceller3> = None;
    // Push to talk keeps sending a moment after the key is let go (so words don't get cut).
    let mut ptt_held_at: Option<Instant> = None;

    while !shared.stop.load(Ordering::Relaxed) {
        match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => buffer.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while buffer.len() >= FRAME_SAMPLES {
            let mut frame: Vec<f32> = buffer.drain(..FRAME_SAMPLES).collect();
            let wants_echo = cancel_echo && shared.config.lock().unwrap().echo_cancellation;
            if wants_echo {
                echo.get_or_insert_with(echo::EchoCanceller3::new).process(&shared.mixer.far_end, &mut frame);
            } else {
                echo = None;
            }
            let settings = {
                let c = shared.config.lock().unwrap();
                let ptt = c.push_to_talk.as_deref().map(|key| {
                    if ptt::is_down(key) {
                        ptt_held_at = Some(Instant::now());
                    }
                    ptt_held_at.is_some_and(|t| t.elapsed() < Duration::from_millis(250))
                });
                CaptureSettings {
                    gain: c.input_gain,
                    // Holding the key sends everything (no voice gate).
                    threshold_db: if ptt == Some(true) { -100.0 } else { c.sensitivity_db },
                    noise_suppression: c.noise_suppression,
                    // Deafening also mutes, like most voice apps.
                    muted: shared.muted.load(Ordering::Relaxed)
                        || shared.deafened.load(Ordering::Relaxed)
                        || ptt == Some(false),
                }
            };
            let out = processor.process(&mut frame, settings);
            shared.input_level.store(out.level_db.to_bits(), Ordering::Relaxed);
            shared.transmitting.store(out.packet.is_some(), Ordering::Relaxed);
            if let Some((seq, opus)) = out.packet {
                send(seq, &opus);
            }
        }
    }
    shared.transmitting.store(false, Ordering::Relaxed);
}

fn open_udp(host: &str, port: u16) -> Option<UdpSocket> {
    if port == 0 {
        return None;
    }
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind).ok()?;
    sock.connect(addr).ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
    Some(sock)
}

/// Keeps the UDP path alive and feeds incoming audio to the mixer.
fn udp_loop(sock: UdpSocket, token: Token, shared: Arc<Shared>) {
    let hello = Packet::Hello { token }.encode();
    let mut last_hello: Option<Instant> = None;
    let mut last_ack: Option<Instant> = None;
    let mut buf = [0u8; 1500];

    while !shared.stop.load(Ordering::Relaxed) {
        let connected = last_ack.is_some_and(|t| t.elapsed() < Duration::from_secs(12));
        shared.udp_ok.store(connected, Ordering::Relaxed);
        // Say hello every second until the server answers, then every 4 s to keep routers' mappings open.
        let interval = if connected { Duration::from_secs(4) } else { Duration::from_secs(1) };
        if last_hello.is_none_or(|t| t.elapsed() >= interval) {
            let _ = sock.send(&hello);
            last_hello = Some(Instant::now());
        }
        match sock.recv(&mut buf) {
            Ok(n) => match Packet::parse(&buf[..n]) {
                Some(Packet::Ack) => {
                    last_ack = Some(Instant::now());
                    // The answer to the last hello: how long the round trip took.
                    if let Some(sent) = last_hello.filter(|t| t.elapsed() < Duration::from_secs(2)) {
                        let now = sent.elapsed().as_millis().min(5000) as u32;
                        let old = shared.ping_ms.load(Ordering::Relaxed);
                        let smooth = if old == u32::MAX { now } else { (old * 3 + now) / 4 };
                        shared.ping_ms.store(smooth, Ordering::Relaxed);
                    }
                }
                Some(Packet::AudioDown { .. } | Packet::SealedDown { .. }) => shared.mixer.push_packet(&buf[..n]),
                Some(Packet::VideoDown { .. }) => {
                    crate::live::deliver(&buf[..n]);
                }
                _ => {}
            },
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            // e.g. "connection refused" when nothing listens on UDP: don't spin.
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    shared.udp_ok.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use celerity_proto::voice::{FRAME_SAMPLES, token_from_hex};
    use celerity_proto::{ChannelKind, ClientMsg, ServerMsg};
    use futures::StreamExt;

    use super::dsp::{CaptureSettings, level_db};
    use super::*;
    use crate::net::{self, Event};

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            stop: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            config: Mutex::new(VoiceConfig {
                input_device: None,
                output_device: None,
                input_gain: 1.0,
                output_volume: 1.0,
                sensitivity_db: -60.0,
                noise_suppression: false,
                echo_cancellation: true,
                push_to_talk: None,
            }),
            input_level: AtomicU32::new(0),
            transmitting: AtomicBool::new(false),
            udp_ok: Arc::new(AtomicBool::new(false)),
            ping_ms: AtomicU32::new(u32::MAX),
            problem: Mutex::new(None),
            mixer: Arc::new(Mixer::default()),
        })
    }

    /// Connect with the app's real network code, register, and join voice (encrypted).
    async fn join(
        address: String,
        name: &str,
        sink: VoiceSink,
    ) -> (Connection, Token, Key, u16, std::pin::Pin<Box<impl futures::Stream<Item = Event>>>, u32) {
        let mut events = Box::pin(net::connect(address, None, sink));
        let Some(Event::Connected { conn, url, fingerprint }) = events.next().await else { panic!("connect failed") };
        assert!(url.starts_with("wss://"), "encrypted connection: {url}");
        assert!(fingerprint.is_some());
        conn.send(ClientMsg::Register { username: name.into(), password: "password123".into(), invite: "x".into() });
        let voice_channel = loop {
            if let Some(Event::Received(msg)) = events.next().await
                && let ServerMsg::Ready(r) = *msg
            {
                break r.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
            }
        };
        conn.send(ClientMsg::JoinVoice { channel_id: voice_channel, encrypted: true });
        loop {
            if let Some(Event::Received(msg)) = events.next().await
                && let ServerMsg::VoiceReady { token, udp_port, key, ssrc, .. } = *msg
            {
                let key = celerity_proto::voice::key_from_hex(&key.expect("a voice key")).unwrap();
                return (conn, token_from_hex(&token).unwrap(), key, udp_port, events, ssrc);
            }
        }
    }

    fn speech_packets(n: usize) -> Vec<(u16, Vec<u8>)> {
        let mut cap = CaptureProcessor::new().unwrap();
        let settings = CaptureSettings { gain: 1.0, threshold_db: -60.0, noise_suppression: false, muted: false };
        let mut phase = 0.0f32;
        (0..n)
            .map(|_| {
                let mut frame: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|_| {
                        phase += 250.0 * std::f32::consts::TAU / 48_000.0;
                        0.3 * phase.sin()
                    })
                    .collect();
                cap.process(&mut frame, settings).packet.unwrap()
            })
            .collect()
    }

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition never became true");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_audio_flows_both_ways_over_udp_and_websocket() {
        use celerity_server::{AppState, Config, db::Db, router, start_udp};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Config { invite_code: "x".into(), ..Config::default() };
        let state = AppState::new(config, Db::open_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        start_udp(&state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        // The real server setup: encrypted and plain connections on one port.
        let dir = std::env::temp_dir().join(format!("celerity-voice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tls, _) = celerity_server::tls::load_or_create(&dir).unwrap();
        let dual = celerity_server::tls::DualListener::new(listener, tls).unwrap();
        tokio::spawn(async move { axum::serve(dual, router(state)).await.unwrap() });

        // Alice only uses the WebSocket; her incoming audio lands in her sink's mixer.
        let alice_mixer = Arc::new(Mixer::default());
        let alice_sink: VoiceSink = Arc::new(Mutex::new(Some(alice_mixer.clone())));
        let (alice_conn, alice_token, alice_key, _, mut alice_events, _) =
            join(address.clone(), "alice", alice_sink).await;
        alice_mixer.set_key(Some(alice_key));

        // Bob uses UDP, through the engine's real UDP loop.
        let (_bob_conn, bob_token, bob_key, udp_port, mut bob_events, _) =
            join(address.clone(), "bob", Arc::default()).await;
        let bob = shared();
        bob.mixer.set_key(Some(bob_key));
        let sock = open_udp("127.0.0.1", udp_port).expect("udp socket");
        let bob_send = sock.try_clone().unwrap();
        let loop_shared = bob.clone();
        let udp_thread = std::thread::spawn(move || udp_loop(sock, bob_token, loop_shared));
        wait_for(|| bob.udp_ok.load(Ordering::Relaxed)).await;

        // Keep both WebSocket streams flowing in the background.
        tokio::spawn(async move { while alice_events.next().await.is_some() {} });
        tokio::spawn(async move { while bob_events.next().await.is_some() {} });

        // Plain audio from someone who encrypts is refused (it could be forged).
        for (seq, opus) in speech_packets(15) {
            alice_conn.send_binary(Packet::AudioUp { token: alice_token, seq, opus: &opus }.encode());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut heard = vec![0.0; FRAME_SAMPLES * 12];
        bob.mixer.fill(&mut heard);
        assert!(level_db(&heard) < -60.0, "plain audio wasn't played");

        // Alice -> server (WebSocket) -> Bob (UDP), sealed both ways.
        for (i, (seq, opus)) in speech_packets(15).into_iter().enumerate() {
            let sealed = seal(&alice_key, Direction::Up, i as u64, seq, &opus);
            alice_conn
                .send_binary(Packet::SealedUp { token: alice_token, counter: i as u64, sealed: &sealed }.encode());
        }
        wait_for(|| {
            bob.mixer.fill(&mut heard);
            level_db(&heard) > -25.0
        })
        .await;

        // Bob -> server (UDP) -> Alice (WebSocket).
        for (i, (seq, opus)) in speech_packets(15).into_iter().enumerate() {
            let sealed = seal(&bob_key, Direction::Up, i as u64, seq, &opus);
            bob_send.send(&Packet::SealedUp { token: bob_token, counter: i as u64, sealed: &sealed }.encode()).unwrap();
        }
        wait_for(|| {
            alice_mixer.fill(&mut heard);
            level_db(&heard) > -25.0
        })
        .await;

        bob.stop.store(true, Ordering::Relaxed);
        udp_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A camera (a test pattern) and a screen share travel from Alice's app over the
    /// WebSocket, through the real server, to Bob's app over UDP, and decode there.
    #[tokio::test(flavor = "multi_thread")]
    async fn video_flows_through_the_server() {
        use celerity_proto::VideoKind;
        use celerity_server::{AppState, Config, db::Db, router, start_udp};
        use celerity_stream as stream;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Config { invite_code: "x".into(), ..Config::default() };
        let state = AppState::new(config, Db::open_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        start_udp(&state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let dir = std::env::temp_dir().join(format!("celerity-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tls, _) = celerity_server::tls::load_or_create(&dir).unwrap();
        let dual = celerity_server::tls::DualListener::new(listener, tls).unwrap();
        tokio::spawn(async move { axum::serve(dual, router(state)).await.unwrap() });

        let (alice_conn, alice_token, alice_key, _, mut alice_events, alice_ssrc) =
            join(address.clone(), "alice", Arc::default()).await;
        let (bob_conn, bob_token, bob_key, udp_port, mut bob_events, _) =
            join(address.clone(), "bob", Arc::default()).await;
        tokio::spawn(async move { while bob_events.next().await.is_some() {} });

        // Bob listens on UDP, with decoders for Alice's camera and screen.
        let bob = shared();
        let sock = open_udp("127.0.0.1", udp_port).expect("udp socket");
        let loop_shared = bob.clone();
        let udp_thread = std::thread::spawn(move || udp_loop(sock, bob_token, loop_shared));
        wait_for(|| bob.udp_ok.load(Ordering::Relaxed)).await;
        crate::live::set_key(Some(bob_key));
        let camera = Arc::new(stream::Receiver::new(stream::ReceiverConfig::new(1)));
        let screen = Arc::new(stream::Receiver::new(stream::ReceiverConfig::new(2)));
        crate::live::route_for_test(alice_ssrc, VideoKind::Camera, camera.clone());
        crate::live::route_for_test(alice_ssrc, VideoKind::Screen, screen.clone());

        // Alice sends over her WebSocket (no UDP).
        let out = crate::live::set_out(crate::live::VideoOut {
            token: alice_token,
            key: alice_key,
            udp: None,
            udp_ok: Arc::new(AtomicBool::new(false)),
            conn: alice_conn.clone(),
            counters: Arc::default(),
        });
        alice_conn.send(celerity_proto::ClientMsg::SetVideo { camera: true, screen: true });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pattern = |w, h| stream::Source::TestPattern { width: w, height: h, fps: 30 };
        let cam = Arc::new(crate::live::start_sender_for_test(
            VideoKind::Camera,
            stream::SenderConfig::camera(1),
            pattern(320, 180),
        ));
        let scr = Arc::new(crate::live::start_sender_for_test(
            VideoKind::Screen,
            stream::SenderConfig::screen(2),
            pattern(640, 360),
        ));
        // Alice's app answers keyframe requests (a new viewer needs one to start from).
        let (c, s) = (cam.clone(), scr.clone());
        tokio::spawn(async move {
            while let Some(e) = alice_events.next().await {
                if let Event::Received(msg) = e
                    && let ServerMsg::KeyframeRequest { kind } = *msg
                {
                    if kind == VideoKind::Camera { c.force_keyframe() } else { s.force_keyframe() }
                }
            }
        });

        // The camera arrives; the screen doesn't until Bob watches it.
        wait_for(|| camera.frame().is_some_and(|f| f.width == 320 && f.height == 180)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(screen.frame().is_none(), "not watching yet");
        let started = Instant::now();
        bob_conn.send(celerity_proto::ClientMsg::WatchStream { user_id: 1, watching: true });
        wait_for(|| screen.frame().is_some_and(|f| f.width == 640)).await;
        assert!(started.elapsed() < Duration::from_secs(2), "the keyframe came on request");

        crate::live::clear_out(out);
        bob.stop.store(true, Ordering::Relaxed);
        udp_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
