//! Cameras and screen shares in calls. The video itself (capture, H.264, packets)
//! is the `syncord-stream` crate; this is the plumbing around it:
//!
//! ```text
//! camera / screen -> stream::Sender --packets--> send() -> sealed, out the call's UDP socket
//!                                                         (or the WebSocket)
//! UDP / WebSocket -> deliver() -> opened, routed by (person, camera|screen) -> stream::Receiver
//! UI tick: Receiver::frame() -> picture on screen; broken pictures ask the sender for a keyframe
//! ```
//!
//! Packets never pass through the UI thread. Cameras go to everyone in the call;
//! screen shares only to people who chose to watch them (the server enforces both).

use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iced::Task;
use iced::widget::image::Handle;
use syncord_proto::voice::{Direction, Key, Packet, Token, open_video, seal_video};
use syncord_proto::{ClientMsg, UserId, VideoKind};
use syncord_stream as stream;

use crate::app::{App, Message};
use crate::net::Connection;
use crate::voice::Counters;

// ---- Packets out

/// How video leaves this computer: set up by the voice engine, which owns the socket.
pub struct VideoOut {
    pub token: Token,
    pub key: Key,
    pub udp: Option<Arc<UdpSocket>>,
    pub udp_ok: Arc<AtomicBool>,
    pub conn: Connection,
    pub counters: Arc<Counters>,
}

static OUT: Mutex<Option<(u64, VideoOut)>> = Mutex::new(None);
static NEXT_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Install where video goes; returns an id for [`clear_out`].
pub fn set_out(out: VideoOut) -> u64 {
    let id = NEXT_OUT.fetch_add(1, Ordering::Relaxed);
    *OUT.lock().unwrap() = Some((id, out));
    id
}

/// Forget the video route, if it's still the one with this id (a restarted engine
/// installs its own before the old one is dropped).
pub fn clear_out(id: u64) {
    let mut out = OUT.lock().unwrap();
    if out.as_ref().is_some_and(|(i, _)| *i == id) {
        *out = None;
    }
}

/// Send one video packet (called on the encoder's thread).
fn send(kind: VideoKind, data: &[u8]) {
    let out = OUT.lock().unwrap();
    let Some((_, o)) = out.as_ref() else { return };
    let counter = o.counters.video.fetch_add(1, Ordering::Relaxed);
    let sealed = seal_video(&o.key, Direction::Up, counter, kind, data);
    let packet = Packet::VideoUp { token: o.token, counter, sealed: &sealed }.encode();
    match &o.udp {
        Some(sock) if o.udp_ok.load(Ordering::Relaxed) => {
            let _ = sock.send(&packet);
        }
        _ => o.conn.send_binary(packet),
    }
}

// ---- Packets in

#[derive(Default)]
struct Inbound {
    key: Option<Key>,
    /// By (the sender's ssrc, which video).
    receivers: HashMap<(u32, VideoKind), Arc<stream::Receiver>>,
}

fn inbound() -> &'static Mutex<Inbound> {
    static IN: std::sync::OnceLock<Mutex<Inbound>> = std::sync::OnceLock::new();
    IN.get_or_init(Default::default)
}

/// The key the server seals video for us with (set when joining a call).
pub fn set_key(key: Option<Key>) {
    inbound().lock().unwrap().key = key;
}

/// Hand a packet from the network to the right decoder. Returns false if it isn't video.
pub fn deliver(data: &[u8]) -> bool {
    let Some(Packet::VideoDown { ssrc, counter, sealed }) = Packet::parse(data) else { return false };
    let Some(key) = inbound().lock().unwrap().key else { return true };
    let Some((kind, payload)) = open_video(&key, Direction::Down, counter, sealed) else { return true };
    let receiver = inbound().lock().unwrap().receivers.get(&(ssrc, kind)).cloned();
    if let Some(r) = receiver {
        r.push_packet(&payload);
    }
    true
}

/// Wakes the UI only when a decoder has a new picture (a 15 fps screen share
/// redraws 15 times a second, and nothing redraws while pictures are frozen).
pub fn new_pictures() -> impl futures::Stream<Item = ()> {
    futures::stream::unfold(HashMap::<(u32, VideoKind), u64>::new(), |mut seen| async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(12)).await;
            let receivers: Vec<_> = inbound().lock().unwrap().receivers.iter().map(|(k, r)| (*k, r.clone())).collect();
            seen.retain(|k, _| receivers.iter().any(|(r, _)| r == k));
            let mut fresh = false;
            for (k, r) in receivers {
                if let Some(f) = r.frame()
                    && seen.insert(k, f.serial) != Some(f.serial)
                {
                    fresh = true;
                }
            }
            if fresh {
                return Some(((), seen));
            }
        }
    })
}

#[cfg(test)]
pub fn route_for_test(ssrc: u32, kind: VideoKind, r: Arc<stream::Receiver>) {
    inbound().lock().unwrap().receivers.insert((ssrc, kind), r);
}

#[cfg(test)]
pub fn start_sender_for_test(kind: VideoKind, cfg: stream::SenderConfig, source: stream::Source) -> stream::Sender {
    stream::Sender::start(cfg, source, move |p| send(kind, p)).unwrap()
}

// ---- The app's side

/// A started sender, passed back from the thread that opened the camera or screen.
#[derive(Clone)]
pub struct Started(Arc<Mutex<Option<stream::Sender>>>);

impl std::fmt::Debug for Started {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Started")
    }
}

/// One video's latest picture.
pub struct Picture {
    serial: u64,
    pub handle: Handle,
}

/// What the screen share picker shows.
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenList {
    Loading,
    Ready(Vec<stream::ScreenInfo>),
    Failed(String),
}

#[derive(Default)]
pub struct Live {
    pub camera: Option<stream::Sender>,
    pub screen: Option<stream::Sender>,
    /// Starting one (the camera or screen is being opened).
    pub starting: HashSet<VideoKind>,
    /// Screen shares you chose to watch.
    pub watching: HashSet<UserId>,
    /// The call view is open (server voice channels; DMs show video in the call bar).
    pub stage: bool,
    /// The video shown big.
    pub focus: Option<(UserId, VideoKind)>,
    receivers: HashMap<(UserId, VideoKind), (u32, Arc<stream::Receiver>)>,
    /// Latest picture per video, yours included (what others see).
    pub pictures: HashMap<(UserId, VideoKind), Picture>,
    pub cameras: Vec<stream::DeviceInfo>,
    /// Smooth motion (720p, 30 fps) rather than sharp text (1080p, 15 fps).
    pub smooth: bool,
}

impl Live {
    /// Pictures are arriving from others.
    pub fn receiving(&self) -> bool {
        !self.receivers.is_empty()
    }

    /// Your camera or screen is on (its preview refreshes).
    pub fn sending_any(&self) -> bool {
        self.camera.is_some() || self.screen.is_some()
    }

    pub fn sending(&self, kind: VideoKind) -> bool {
        match kind {
            VideoKind::Camera => self.camera.is_some(),
            VideoKind::Screen => self.screen.is_some(),
        }
    }
}

/// Wrap a frame's pixels for iced without copying them.
fn handle_of(f: &stream::Frame) -> Handle {
    struct Pixels(Arc<Vec<u8>>);
    impl AsRef<[u8]> for Pixels {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
    Handle::from_rgba(f.width, f.height, bytes::Bytes::from_owner(Pixels(f.rgba.clone())))
}

fn stream_id(kind: VideoKind) -> u32 {
    kind.to_u8() as u32
}

impl App {
    fn in_call(&self) -> bool {
        self.voice.is_some()
    }

    fn tell_video_state(&self) {
        self.send(ClientMsg::SetVideo { camera: self.live.camera.is_some(), screen: self.live.screen.is_some() });
    }

    pub(crate) fn update_live(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleCamera => {
                if self.live.camera.take().is_some() {
                    self.tell_video_state();
                    return Task::none();
                }
                if !self.in_call() || self.live.starting.contains(&VideoKind::Camera) {
                    return Task::none();
                }
                self.live.starting.insert(VideoKind::Camera);
                let mut cfg = stream::SenderConfig::camera(stream_id(VideoKind::Camera));
                cfg.preview_max_side = Some(480);
                let wanted = self.settings.camera.clone();
                return start(VideoKind::Camera, cfg, move || {
                    Ok(stream::Source::Camera {
                        id: camera_id(wanted.as_deref())?,
                        config: stream::CameraConfig::default(),
                    })
                });
            }
            Message::OpenScreenPicker => {
                if self.live.screen.take().is_some() {
                    // The same button stops sharing.
                    self.tell_video_state();
                    return Task::none();
                }
                if !self.in_call() {
                    return Task::none();
                }
                self.close_overlay();
                self.overlay = Some(crate::app::Overlay::ScreenPicker(ScreenList::Loading));
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(stream::list_screens).await.unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::ScreensListed,
                );
            }
            Message::ScreensListed(list) => {
                if let Some(crate::app::Overlay::ScreenPicker(shown)) = &mut self.overlay {
                    *shown = match list {
                        Ok(screens) if screens.is_empty() => ScreenList::Failed("Nothing to share was found.".into()),
                        Ok(screens) => ScreenList::Ready(screens),
                        Err(e) => ScreenList::Failed(e),
                    };
                }
            }
            Message::ToggleSmoothScreen(on) => self.live.smooth = on,
            Message::ShareScreen(id) => {
                self.close_overlay();
                if !self.in_call() || self.live.starting.contains(&VideoKind::Screen) {
                    return Task::none();
                }
                self.live.starting.insert(VideoKind::Screen);
                let mut cfg = stream::SenderConfig::screen(stream_id(VideoKind::Screen));
                cfg.preview_max_side = Some(480);
                let config = if self.live.smooth {
                    cfg.max_width = 1280;
                    cfg.max_height = 720;
                    cfg.encoder.max_fps = 30.0;
                    stream::ScreenConfig { max_width: 1280, max_height: 720, fps: 30, show_cursor: true }
                } else {
                    stream::ScreenConfig { max_width: 1920, max_height: 1080, fps: 15, show_cursor: true }
                };
                return start(VideoKind::Screen, cfg, move || Ok(stream::Source::Screen { id, config }));
            }
            Message::VideoStarted(kind, result) => {
                self.live.starting.remove(&kind);
                let sender = match result {
                    Ok(started) => started.0.lock().unwrap().take(),
                    Err(e) => {
                        self.notice = Some(match kind {
                            VideoKind::Camera => format!("Couldn't start your camera: {e}"),
                            VideoKind::Screen => format!("Couldn't share your screen: {e}"),
                        });
                        None
                    }
                };
                // Left the call while it was starting: just drop it.
                if let Some(sender) = sender.filter(|_| self.in_call()) {
                    match kind {
                        VideoKind::Camera => self.live.camera = Some(sender),
                        VideoKind::Screen => {
                            self.live.screen = Some(sender);
                            self.live.stage = true;
                        }
                    }
                    self.tell_video_state();
                }
            }
            Message::WatchScreen(user) => {
                self.live.watching.insert(user);
                self.live.stage = true;
                self.live.focus = Some((user, VideoKind::Screen));
                self.send(ClientMsg::WatchStream { user_id: user, watching: true });
                self.sync_live();
            }
            Message::StopWatching(user) => {
                self.live.watching.remove(&user);
                if self.live.focus == Some((user, VideoKind::Screen)) {
                    self.live.focus = None;
                }
                self.send(ClientMsg::WatchStream { user_id: user, watching: false });
                self.sync_live();
            }
            Message::OpenStage => self.live.stage = true,
            Message::CloseStage => self.live.stage = false,
            Message::FocusVideo(target) => {
                self.live.focus = if self.live.focus == target { None } else { target };
                self.sync_live();
            }
            Message::CameraDevice(name) => {
                self.settings.camera = (name != crate::app::DEFAULT_DEVICE).then_some(name);
                self.settings.save();
                // Switch over if it's on.
                if self.live.camera.take().is_some() {
                    return self.update_live(Message::ToggleCamera);
                }
            }
            Message::LiveTick => self.pull_pictures(),
            _ => {}
        }
        Task::none()
    }

    /// Match the decoders to who's in the call with their camera on (or a screen
    /// you're watching), ask for keyframes where pictures broke up, and notice
    /// cameras or screens that stopped by themselves.
    pub(crate) fn sync_live(&mut self) {
        let Some(s) = &self.session else { return };
        let me = s.me;
        let Some(channel) = self.voice.as_ref().map(|v| v.channel_id) else { return };

        let mut wanted: HashMap<(UserId, VideoKind), u32> = HashMap::new();
        for v in s.voice.values().filter(|v| v.channel_id == channel && v.user_id != me) {
            if v.camera {
                wanted.insert((v.user_id, VideoKind::Camera), v.ssrc);
            }
            if v.screen && self.live.watching.contains(&v.user_id) {
                wanted.insert((v.user_id, VideoKind::Screen), v.ssrc);
            }
        }
        // People who stopped sharing aren't being watched any more.
        let sharing: HashSet<UserId> =
            s.voice.values().filter(|v| v.channel_id == channel && v.screen).map(|v| v.user_id).collect();
        self.live.watching.retain(|u| sharing.contains(u));
        if let Some((u, k)) = self.live.focus {
            let still = u == me && self.live.sending(k)
                || match k {
                    VideoKind::Camera => wanted.contains_key(&(u, k)),
                    VideoKind::Screen => sharing.contains(&u),
                };
            if !still {
                self.live.focus = None;
            }
        }

        let before = self.live.receivers.len();
        self.live.receivers.retain(|key, (ssrc, _)| wanted.get(key) == Some(ssrc));
        let mut changed = before != self.live.receivers.len();
        for (key, ssrc) in &wanted {
            if !self.live.receivers.contains_key(key) {
                let mut cfg = stream::ReceiverConfig::new(stream_id(key.1));
                cfg.max_side = if self.live.focus == Some(*key) { 1920 } else { 640 };
                self.live.receivers.insert(*key, (*ssrc, Arc::new(stream::Receiver::new(cfg))));
                changed = true;
            }
        }
        if changed {
            let mut inb = inbound().lock().unwrap();
            inb.receivers =
                self.live.receivers.iter().map(|((_, kind), (ssrc, r))| ((*ssrc, *kind), r.clone())).collect();
        }
        let (camera, screen) = (self.live.camera.is_some(), self.live.screen.is_some());
        let receivers = &self.live.receivers;
        self.live.pictures.retain(|(u, k), _| {
            let mine = *u == me && if *k == VideoKind::Camera { camera } else { screen };
            mine || receivers.contains_key(&(*u, *k))
        });

        for ((user, kind), (_, r)) in &self.live.receivers {
            // Full size only for the video shown big: small tiles decode small (less to draw).
            r.set_max_side(if self.live.focus == Some((*user, *kind)) { 1920 } else { 640 });
            if r.take_keyframe_request() {
                self.send(ClientMsg::RequestKeyframe { user_id: *user, kind: *kind });
            }
        }

        let mut stopped = Vec::new();
        for (kind, sender) in [(VideoKind::Camera, &self.live.camera), (VideoKind::Screen, &self.live.screen)] {
            if let Some(e) = sender.as_ref().and_then(|s| s.error()) {
                stopped.push((kind, e));
            }
        }
        for (kind, e) in stopped {
            match kind {
                VideoKind::Camera => self.live.camera = None,
                VideoKind::Screen => self.live.screen = None,
            }
            self.notice = Some(match kind {
                VideoKind::Camera => format!("Your camera stopped: {e}"),
                VideoKind::Screen => format!("Screen sharing stopped: {e}"),
            });
            self.tell_video_state();
        }
    }

    /// New pictures from the decoders (and of what you're sending).
    fn pull_pictures(&mut self) {
        let Some(me) = self.session.as_ref().map(|s| s.me) else { return };
        let mut fresh: Vec<((UserId, VideoKind), stream::Frame)> = Vec::new();
        for (key, (_, r)) in &self.live.receivers {
            if let Some(f) = r.frame() {
                fresh.push((*key, f));
            }
        }
        for (kind, sender) in [(VideoKind::Camera, &self.live.camera), (VideoKind::Screen, &self.live.screen)] {
            if let Some(f) = sender.as_ref().and_then(|s| s.preview()) {
                fresh.push(((me, kind), f));
            }
        }
        for (key, f) in fresh {
            if self.live.pictures.get(&key).is_some_and(|p| p.serial == f.serial) {
                continue;
            }
            let picture = Picture { serial: f.serial, handle: handle_of(&f) };
            self.live.pictures.insert(key, picture);
        }
    }

    pub(crate) fn on_keyframe_request(&self, kind: VideoKind) {
        let sender = match kind {
            VideoKind::Camera => &self.live.camera,
            VideoKind::Screen => &self.live.screen,
        };
        if let Some(s) = sender {
            s.force_keyframe();
        }
    }

    /// Leaving the call: stop everything.
    pub(crate) fn stop_live(&mut self) {
        let cameras = std::mem::take(&mut self.live.cameras);
        let smooth = self.live.smooth;
        self.live = Live { cameras, smooth, ..Default::default() };
        inbound().lock().unwrap().receivers.clear();
    }

    /// For the camera picker in voice settings.
    pub(crate) fn refresh_cameras(&mut self) {
        self.live.cameras = stream::list_cameras().unwrap_or_default();
    }
}

/// The camera to use: the one picked in settings (by name), else the first.
fn camera_id(wanted: Option<&str>) -> Result<String, String> {
    let cameras = stream::list_cameras()?;
    cameras
        .iter()
        .find(|c| Some(c.name.as_str()) == wanted)
        .or(cameras.first())
        .map(|c| c.id.clone())
        .ok_or_else(|| "No camera found. Is one plugged in?".to_string())
}

/// Open the camera or screen on a background thread (it can take a moment).
fn start(
    kind: VideoKind,
    cfg: stream::SenderConfig,
    source: impl FnOnce() -> Result<stream::Source, String> + Send + 'static,
) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                stream::Sender::start(cfg, source()?, move |packet| send(kind, packet))
                    .map(|s| Started(Arc::new(Mutex::new(Some(s)))))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        },
        move |r| Message::VideoStarted(kind, r),
    )
}
