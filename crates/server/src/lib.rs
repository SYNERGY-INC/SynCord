//! Celerity server library: the binary in `main.rs` is a thin wrapper so the
//! server can also be started from integration tests.

pub mod calls;
pub mod db;
pub mod embeds;
pub mod gateway;
pub mod gifs;
pub mod http;
pub mod media;
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
    /// Install new server versions automatically (Windows), or just log them (elsewhere).
    pub auto_update: bool,
    /// Where to look for new versions.
    pub update_url: String,
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
            auto_update: true,
            update_url: celerity_update::MANIFEST_URL.into(),
        }
    }
}

/// One event for logged-in connections, already turned into JSON.
#[derive(Clone)]
pub struct Event {
    pub json: Arc<str>,
    /// Who should get it; `None` means everyone (direct messages go only to their two members).
    pub to: Option<Arc<[UserId]>>,
}

impl Event {
    pub fn is_for(&self, user: UserId) -> bool {
        self.to.as_ref().is_none_or(|to| to.contains(&user))
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
            previews: if config.unsafe_allow_private_link_previews {
                embeds::Fetcher::new_for_tests()
            } else {
                embeds::Fetcher::new()
            },
            db,
            config,
            events,
            online: Mutex::new(HashMap::new()),
            voice: Arc::default(),
            udp_port: AtomicU16::new(0),
            next_conn_id: AtomicU64::new(1),
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
        let json: Arc<str> = serde_json::to_string(msg).expect("serialize").into();
        // Err only means nobody is listening right now.
        let _ = self.events.send(Event { json, to });
    }
}

/// Bind the UDP voice socket and start relaying. Returns the bound port.
pub async fn start_udp(state: &Arc<AppState>, addr: SocketAddr) -> std::io::Result<u16> {
    let socket = Arc::new(tokio::net::UdpSocket::bind(addr).await?);
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
        .with_state(state)
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
