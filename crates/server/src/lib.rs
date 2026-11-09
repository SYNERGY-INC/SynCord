//! Celerity server library: the binary in `main.rs` is a thin wrapper so the
//! server can also be started from integration tests.

pub mod admin;
pub mod audio;
pub mod backup;
pub mod calls;
pub mod db;
pub mod embeds;
pub mod expressions;
pub mod gateway;
pub mod gifs;
pub mod http;
pub mod media;
pub mod moderation;
pub mod previews;
pub mod tls;
pub mod update;
pub mod voice;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::get;
use celerity_proto::UserId;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address and port to listen on. `0.0.0.0` means "every network card".
    pub bind: String,
    /// Name shown at the top of the channel list.
    pub community_name: String,
    /// Code friends must enter to create an account. Leave empty to allow anyone.
    pub invite_code: String,
    /// Path of the SQLite database file.
    pub database: PathBuf,
    /// Folder where uploaded files are kept.
    pub uploads: PathBuf,
    /// Largest file people can upload, in megabytes.
    pub max_upload_mb: u64,
    /// GIF search: a free key from developers.giphy.com (or use KLIPY below).
    pub giphy_api_key: String,
    /// GIF search: a free key from partner.klipy.com.
    pub klipy_api_key: String,
    /// Only for testing: a different address for the GIF service.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub gif_api_base: String,
    /// For tests only: let link previews reach this computer and the home network.
    /// Never turn this on for a real server: anyone could use it to poke at your network.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unsafe_allow_private_link_previews: bool,
    /// Only for testing: ask this address instead of YouTube, X/Twitter and Spotify.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub preview_api_base: String,
    /// Install new server versions automatically (Windows), or just log them (elsewhere).
    pub auto_update: bool,
    /// Where to look for new versions.
    pub update_url: String,
    /// Copy the database into a `backups` folder next to it once a day (the newest 7 are kept).
    pub backups: bool,
    /// The address friends use to reach this server (`host:port`), used in invite links.
    /// Leave empty to use whatever address the person making the link connected to.
    pub public_address: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7878".into(),
            community_name: "Celerity".into(),
            invite_code: gateway::random_token(6),
            database: "celerity.db".into(),
            uploads: "uploads".into(),
            max_upload_mb: 1024,
            giphy_api_key: String::new(),
            klipy_api_key: String::new(),
            gif_api_base: String::new(),
            unsafe_allow_private_link_previews: false,
            preview_api_base: String::new(),
            auto_update: true,
            update_url: celerity_update::MANIFEST_URL.into(),
            backups: true,
            public_address: String::new(),
        }
    }
}

/// Who an event goes to.
#[derive(Debug, Clone, Default)]
pub struct Target {
    /// `None` means everyone; otherwise only these people (direct messages, groups, private channels).
    pub users: Option<Arc<[UserId]>>,
    /// Only apps from 0.7 on, which can show it (thread and group messages would only
    /// confuse older apps, which don't know those channels).
    pub modern_only: bool,
}

impl Target {
    pub fn everyone() -> Self {
        Self::default()
    }

    pub fn only(users: impl Into<Arc<[UserId]>>) -> Self {
        Self { users: Some(users.into()), modern_only: false }
    }

    pub fn modern(mut self, modern_only: bool) -> Self {
        self.modern_only |= modern_only;
        self
    }

    pub fn includes(&self, user: UserId) -> bool {
        self.users.as_ref().is_none_or(|to| to.contains(&user))
    }
}

impl From<Option<Arc<[UserId]>>> for Target {
    fn from(users: Option<Arc<[UserId]>>) -> Self {
        Self { users, modern_only: false }
    }
}

/// One event for logged-in connections, already turned into JSON.
#[derive(Clone)]
pub struct Event {
    pub json: Arc<str>,
    pub to: Target,
    /// After delivering it, close this person's connections (they were kicked or banned).
    pub disconnect: Option<UserId>,
}

impl Event {
    /// Whether a connection of `user` speaking `protocol` should get it.
    pub fn is_for(&self, user: UserId, protocol: u32) -> bool {
        self.to.includes(user) && (!self.to.modern_only || protocol >= celerity_proto::PROTOCOL_VERSION)
    }
}

/// Shared state for every connection.
pub struct AppState {
    pub db: db::Db,
    pub config: Config,
    /// Fan-out of events to every logged-in connection, pre-serialized to JSON.
    pub events: broadcast::Sender<Event>,
    /// Number of open connections per user (a user may be logged in on several devices).
    pub online: Mutex<HashMap<UserId, usize>>,
    pub voice: Arc<voice::Voice>,
    /// GIF search, if a key is configured.
    pub gifs: Option<gifs::GifSearch>,
    /// Link previews.
    pub previews: embeds::Fetcher,
    /// Calls going on in direct messages.
    pub calls: calls::Calls,
    /// Everyone's chosen status and activity (kept while the server runs).
    pub presence: Mutex<HashMap<UserId, celerity_proto::PresenceInfo>>,
    /// UDP port voice is served on (0 until [`start_udp`] runs).
    pub udp_port: AtomicU16,
    pub next_conn_id: AtomicU64,
    /// Signed-in sessions of the admin web page.
    pub admin: admin::Sessions,
    /// When each person last played soundboard sounds (for the rate limit).
    pub soundboard: Mutex<HashMap<UserId, std::collections::VecDeque<std::time::Instant>>>,
}

impl AppState {
    pub fn new(config: Config, db: db::Db) -> Arc<Self> {
        // HTTPS (GIF search, updates) and encrypted connections use the same small crypto library.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (events, _) = broadcast::channel(1024);
        let gifs = gifs::GifSearch::from_config(&config);
        Arc::new(Self {
            gifs,
            calls: calls::Calls::default(),
            presence: Mutex::new(HashMap::new()),
            previews: {
                let fetcher = if config.unsafe_allow_private_link_previews {
                    embeds::Fetcher::new_for_tests()
                } else {
                    embeds::Fetcher::new()
                };
                if config.preview_api_base.is_empty() {
                    fetcher
                } else {
                    fetcher.with_providers(embeds::Providers::all_at(&config.preview_api_base))
                }
            },
            db,
            config,
            events,
            online: Mutex::new(HashMap::new()),
            voice: Arc::default(),
            udp_port: AtomicU16::new(0),
            next_conn_id: AtomicU64::new(1),
            admin: admin::Sessions::default(),
            soundboard: Mutex::new(HashMap::new()),
        })
    }
}

impl AppState {
    /// Someone's status and activity (online with nothing going on, unless they said otherwise).
    pub fn presence_of(&self, user: UserId) -> celerity_proto::PresenceInfo {
        self.presence.lock().unwrap().get(&user).cloned().unwrap_or(celerity_proto::PresenceInfo {
            user_id: user,
            status: celerity_proto::Status::Online,
            activity: None,
        })
    }

    /// What everyone else is told about someone who's connected.
    pub fn presence_msg(&self, user: UserId) -> celerity_proto::ServerMsg {
        let p = self.presence_of(user);
        let invisible = p.status == celerity_proto::Status::Invisible;
        celerity_proto::ServerMsg::Presence {
            user_id: user,
            online: !invisible,
            status: if invisible { celerity_proto::Status::Online } else { p.status },
            activity: if invisible { None } else { p.activity },
        }
    }

    /// Send an event to everyone (`None`) or only to the listed people.
    pub fn broadcast(&self, msg: &celerity_proto::ServerMsg, to: Option<Arc<[UserId]>>) {
        self.broadcast_target(msg, to.into());
    }

    pub fn broadcast_target(&self, msg: &celerity_proto::ServerMsg, to: Target) {
        let json: Arc<str> = serde_json::to_string(msg).expect("serialize").into();
        // Err only means nobody is listening right now.
        let _ = self.events.send(Event { json, to, disconnect: None });
    }

    /// Tell everyone, then close every connection of `user` (kicked or banned).
    pub fn broadcast_and_disconnect(&self, msg: &celerity_proto::ServerMsg, user: UserId) {
        let json: Arc<str> = serde_json::to_string(msg).expect("serialize").into();
        let _ = self.events.send(Event { json, to: Target::everyone(), disconnect: Some(user) });
    }

    /// Who gets a channel's events (see [`db::channel_audience`]); threads and groups only
    /// go to apps that know them.
    pub async fn channel_target(&self, channel: celerity_proto::ChannelId) -> Result<Target, rusqlite::Error> {
        self.db
            .call(move |c| {
                let kind = db::channel_kind(c, channel)?;
                let users = db::channel_audience(c, channel)?.map(Arc::from);
                let modern =
                    matches!(kind, Some(celerity_proto::ChannelKind::Group | celerity_proto::ChannelKind::Thread));
                Ok(Target { users, modern_only: modern })
            })
            .await
    }
}

/// Bind the UDP voice socket and start relaying. Returns the bound port.
pub async fn start_udp(state: &Arc<AppState>, addr: SocketAddr) -> std::io::Result<u16> {
    let socket = tokio::net::UdpSocket::bind(addr).await?;
    // High-quality screen shares come in bursts at every keyframe; bigger buffers keep
    // the operating system from dropping them before the relay gets to them.
    let sock = socket2::SockRef::from(&socket);
    for result in
        [sock.set_recv_buffer_size(voice::UDP_BUFFER_BYTES), sock.set_send_buffer_size(voice::UDP_BUFFER_BYTES)]
    {
        if let Err(e) = result {
            tracing::debug!("couldn't enlarge the UDP buffers: {e}");
        }
    }
    let socket = Arc::new(socket);
    let port = socket.local_addr()?.port();
    state.voice.set_udp(socket.clone());
    state.udp_port.store(port, Ordering::Relaxed);
    tokio::spawn(voice::run_udp(state.voice.clone(), socket));
    Ok(port)
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { "Celerity server is running.\n" }))
        .route("/ws", get(gateway::upgrade))
        // Uploads are streamed to disk and size-checked as they arrive.
        .route(
            celerity_proto::UPLOAD_PATH,
            axum::routing::post(http::upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(&format!("{}/{{id}}", celerity_proto::FILES_PATH), get(http::download))
        .route(
            celerity_proto::PROFILE_IMAGE_PATH,
            axum::routing::post(http::upload_profile_image)
                .layer(axum::extract::DefaultBodyLimit::max(http::MAX_PROFILE_IMAGE as usize)),
        )
        .route(&format!("{}/{{id}}", celerity_proto::PROFILE_IMAGE_PATH), get(http::download_profile_image))
        .route(
            celerity_proto::expressions::EMOJI_PATH,
            axum::routing::post(expressions::upload_emoji)
                .layer(axum::extract::DefaultBodyLimit::max(expressions::MAX_SOURCE_PICTURE)),
        )
        .route(&format!("{}/{{id}}", celerity_proto::expressions::EMOJI_PATH), get(expressions::download_emoji))
        .route(
            celerity_proto::expressions::STICKERS_PATH,
            axum::routing::post(expressions::upload_sticker)
                .layer(axum::extract::DefaultBodyLimit::max(expressions::MAX_SOURCE_PICTURE)),
        )
        .route(&format!("{}/{{id}}", celerity_proto::expressions::STICKERS_PATH), get(expressions::download_sticker))
        .route(
            celerity_proto::expressions::SOUNDS_PATH,
            axum::routing::post(expressions::upload_sound)
                .layer(axum::extract::DefaultBodyLimit::max(celerity_proto::expressions::MAX_SOUND_BYTES + 1024)),
        )
        .route(&format!("{}/{{id}}", celerity_proto::expressions::SOUNDS_PATH), get(expressions::download_sound))
        .route(&format!("{}/{{token}}", celerity_proto::invites::INVITE_PAGE_PATH), get(admin::invite_page))
        .merge(admin::routes())
        .with_state(state)
}

/// Everything that runs on a timer: upload cleanup, ending polls, and daily backups.
pub fn start_background(state: Arc<AppState>) {
    start_cleanup(state.clone());
    start_polls(state.clone());
    backup::start(state);
}

/// Every few seconds, announce the final results of polls whose time ran out.
pub fn start_polls(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            if let Err(e) = gateway::close_due_polls(&state).await {
                tracing::warn!("closing polls failed: {e}");
            }
        }
    });
}

/// Every hour, delete uploads that were never sent in a message (after a day).
pub fn start_cleanup(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let cutoff = db::now_ms() - 24 * 3600 * 1000;
            match state.db.call(move |c| db::take_stale_uploads(c, cutoff)).await {
                Ok(ids) => http::remove_files(&state.config.uploads, &ids).await,
                Err(e) => tracing::warn!("upload cleanup failed: {e}"),
            }
        }
    });
}
