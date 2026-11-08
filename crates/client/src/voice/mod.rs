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
pub mod jitter;
pub mod mixer;

use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use syncord_proto::voice::{FRAME_SAMPLES, Packet, Token};

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
}

/// How audio leaves this computer.
pub enum Link {
    /// Talk to the server: UDP if it gets through, otherwise the WebSocket.
    Server { host: String, udp_port: u16, token: Token, conn: Connection },
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
}

struct Shared {
    stop: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    config: Mutex<VoiceConfig>,
    input_level: AtomicU32,
    transmitting: AtomicBool,
    udp_ok: AtomicBool,
    problem: Mutex<Option<String>>,
    mixer: Arc<Mixer>,
}

pub struct VoiceEngine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl VoiceEngine {
    pub fn start(config: VoiceConfig, link: Link, muted: bool, deafened: bool) -> Result<Self, String> {
        let mixer = Arc::new(Mixer::default());
        mixer.set_volume(config.output_volume);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            muted: AtomicBool::new(muted),
            deafened: AtomicBool::new(deafened),
            config: Mutex::new(config.clone()),
            input_level: AtomicU32::new((-100f32).to_bits()),
            transmitting: AtomicBool::new(false),
            udp_ok: AtomicBool::new(false),
            problem: Mutex::new(None),
            mixer: mixer.clone(),
        });
        let mut threads = Vec::new();

        // Audio devices live on their own thread: cpal streams can't move between threads on every OS.
        let (chunks_tx, chunks_rx) = mpsc::sync_channel::<Vec<f32>>(32);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        {
            let shared = shared.clone();
            threads.push(spawn("syncord-audio", move || {
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
            Link::Server { host, udp_port, token, conn } => {
                let udp = open_udp(&host, udp_port);
                if let Some(sock) = &udp {
                    let shared = shared.clone();
                    let sock = sock.try_clone().expect("clone udp socket");
                    threads.push(spawn("syncord-voice-udp", move || udp_loop(sock, token, shared)));
                }
                let udp = udp.and_then(|s| s.try_clone().ok());
                let shared = shared.clone();
                Box::new(move |seq, opus| {
                    let packet = Packet::AudioUp { token, seq, opus }.encode();
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
        threads.push(spawn("syncord-capture", move || capture_loop(chunks_rx, capture_shared, sender)));

        Ok(Self { shared, threads })
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
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn audio thread")
}

fn capture_loop(chunks: mpsc::Receiver<Vec<f32>>, shared: Arc<Shared>, mut send: PacketSender) {
    let mut processor = match CaptureProcessor::new() {
        Ok(p) => p,
        Err(e) => {
            *shared.problem.lock().unwrap() = Some(format!("Voice encoder failed to start: {e}"));
            return;
        }
    };
    let mut buffer: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);

    while !shared.stop.load(Ordering::Relaxed) {
        match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => buffer.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while buffer.len() >= FRAME_SAMPLES {
            let mut frame: Vec<f32> = buffer.drain(..FRAME_SAMPLES).collect();
            let settings = {
                let c = shared.config.lock().unwrap();
                CaptureSettings {
                    gain: c.input_gain,
                    threshold_db: c.sensitivity_db,
                    noise_suppression: c.noise_suppression,
                    // Deafening also mutes, like most voice apps.
                    muted: shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed),
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
                Some(Packet::Ack) => last_ack = Some(Instant::now()),
                Some(Packet::AudioDown { ssrc, seq, opus }) => shared.mixer.push(ssrc, seq, opus),
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
    use futures::StreamExt;
    use syncord_proto::voice::{FRAME_SAMPLES, token_from_hex};
    use syncord_proto::{ChannelKind, ClientMsg, ServerMsg};

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
            }),
            input_level: AtomicU32::new(0),
            transmitting: AtomicBool::new(false),
            udp_ok: AtomicBool::new(false),
            problem: Mutex::new(None),
            mixer: Arc::new(Mixer::default()),
        })
    }

    /// Connect with the app's real network code, register, and join voice.
    async fn join(
        url: String,
        name: &str,
        sink: VoiceSink,
    ) -> (Connection, Token, u16, std::pin::Pin<Box<impl futures::Stream<Item = Event>>>) {
        let mut events = Box::pin(net::connect(url, sink));
        let Some(Event::Connected(conn)) = events.next().await else { panic!("connect failed") };
        conn.send(ClientMsg::Register { username: name.into(), password: "password123".into(), invite: "x".into() });
        let voice_channel = loop {
            if let Some(Event::Received(msg)) = events.next().await
                && let ServerMsg::Ready(r) = *msg
            {
                break r.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
            }
        };
        conn.send(ClientMsg::JoinVoice { channel_id: voice_channel });
        loop {
            if let Some(Event::Received(msg)) = events.next().await
                && let ServerMsg::VoiceReady { token, udp_port, .. } = *msg
            {
                return (conn, token_from_hex(&token).unwrap(), udp_port, events);
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
    async fn audio_flows_both_ways_over_udp_and_websocket() {
        use syncord_server::{AppState, Config, db::Db, router, start_udp};
        let config = Config { invite_code: "x".into(), ..Config::default() };
        let state = AppState::new(config, Db::open_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        start_udp(&state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

        // Alice only uses the WebSocket; her incoming audio lands in her sink's mixer.
        let alice_mixer = Arc::new(Mixer::default());
        let alice_sink: VoiceSink = Arc::new(Mutex::new(Some(alice_mixer.clone())));
        let (alice_conn, alice_token, _, mut alice_events) = join(url.clone(), "alice", alice_sink).await;

        // Bob uses UDP, through the engine's real UDP loop.
        let (_bob_conn, bob_token, udp_port, mut bob_events) = join(url.clone(), "bob", Arc::default()).await;
        let bob = shared();
        let sock = open_udp("127.0.0.1", udp_port).expect("udp socket");
        let bob_send = sock.try_clone().unwrap();
        let loop_shared = bob.clone();
        let udp_thread = std::thread::spawn(move || udp_loop(sock, bob_token, loop_shared));
        wait_for(|| bob.udp_ok.load(Ordering::Relaxed)).await;

        // Keep both WebSocket streams flowing in the background.
        tokio::spawn(async move { while alice_events.next().await.is_some() {} });
        tokio::spawn(async move { while bob_events.next().await.is_some() {} });

        // Alice -> server (WebSocket) -> Bob (UDP).
        for (seq, opus) in speech_packets(15) {
            alice_conn.send_binary(Packet::AudioUp { token: alice_token, seq, opus: &opus }.encode());
        }
        let mut heard = vec![0.0; FRAME_SAMPLES * 12];
        wait_for(|| {
            bob.mixer.fill(&mut heard);
            level_db(&heard) > -25.0
        })
        .await;

        // Bob -> server (UDP) -> Alice (WebSocket).
        for (seq, opus) in speech_packets(15) {
            bob_send.send(&Packet::AudioUp { token: bob_token, seq, opus: &opus }.encode()).unwrap();
        }
        wait_for(|| {
            alice_mixer.fill(&mut heard);
            level_db(&heard) > -25.0
        })
        .await;

        bob.stop.store(true, Ordering::Relaxed);
        udp_thread.join().unwrap();
    }
}
