//! SynCord server library: the binary in `main.rs` is a thin wrapper so the
//! server can also be started from integration tests.

pub mod db;
pub mod gateway;

use std::collections::HashMap;
use std::path::PathBuf;
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
}

impl AppState {
    pub fn new(config: Config, db: db::Db) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self { db, config, events, online: Mutex::new(HashMap::new()) })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { "SynCord server is running.\n" }))
        .route("/ws", get(gateway::upgrade))
        .with_state(state)
}
