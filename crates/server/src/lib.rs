//! SynCord server library: the binary in `main.rs` is a thin wrapper so the
//! server can also be started from integration tests.

pub mod db;
pub mod gateway;
pub mod http;
pub mod media;
pub mod voice;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use syncord_proto::UserId;
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7878".into(),
            community_name: "SynCord".into(),
            invite_code: gateway::random_token(6),
            database: "syncord.db".into(),
            uploads: "uploads".into(),
            max_upload_mb: 25,
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
    /// UDP port voice is served on (0 until [`start_udp`] runs).
    pub udp_port: AtomicU16,
    pub next_conn_id: AtomicU64,
}

impl AppState {
    pub fn new(config: Config, db: db::Db) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
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
    let max_upload = (state.config.max_upload_mb as usize).saturating_mul(1024 * 1024).max(1024);
    Router::new()
        .route("/", get(|| async { "SynCord server is running.\n" }))
        .route("/ws", get(gateway::upgrade))
        .route(
            syncord_proto::UPLOAD_PATH,
            axum::routing::post(http::upload).layer(axum::extract::DefaultBodyLimit::max(max_upload)),
        )
        .route(&format!("{}/{{id}}", syncord_proto::FILES_PATH), get(http::download))
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
