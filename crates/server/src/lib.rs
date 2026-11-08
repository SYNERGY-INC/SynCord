//! SynCord server library: the binary in `main.rs` is a thin wrapper so the
//! server can also be started from integration tests.

pub mod db;
pub mod gateway;
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7878".into(),
            community_name: "SynCord".into(),
            invite_code: gateway::random_token(6),
            database: "syncord.db".into(),
        }
    }
}

/// Shared state for every connection.
pub struct AppState {
    pub db: db::Db,
    pub config: Config,
    /// Fan-out of events to every logged-in connection, pre-serialized to JSON.
    pub events: broadcast::Sender<Arc<str>>,
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
    Router::new()
        .route("/", get(|| async { "SynCord server is running.\n" }))
        .route("/ws", get(gateway::upgrade))
        .with_state(state)
}
