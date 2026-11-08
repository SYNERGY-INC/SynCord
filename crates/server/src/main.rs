//! SynCord server: one small binary that stores everything in a single SQLite
//! file and talks to clients over a WebSocket at `/ws`.

use std::net::SocketAddr;
use std::path::PathBuf;

use syncord_server::{AppState, Config, db, router};
use tracing::{info, warn};

const CONFIG_FILE: &str = "syncord-server.toml";

fn load_config() -> Config {
    let path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| CONFIG_FILE.into());
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("error: could not parse {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        Err(_) => {
            let cfg = Config::default();
            let text = format!(
                "# SynCord server settings. Restart the server after editing.\n{}",
                toml::to_string_pretty(&cfg).unwrap()
            );
            match std::fs::write(&path, text) {
                Ok(()) => info!("wrote default config to {}", path.display()),
                Err(e) => warn!("could not write {}: {e}", path.display()),
            }
            cfg
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = load_config();
    let db = match db::Db::open(&config.database) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: could not open database {}: {e}", config.database.display());
            std::process::exit(1);
        }
    };

    let addr: SocketAddr = match config.bind.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: invalid bind address {:?}: {e}", config.bind);
            std::process::exit(1);
        }
    };

    let state = AppState::new(config, db);
    let app = router(state.clone());

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: could not listen on {addr}: {e}");
            std::process::exit(1);
        }
    };

    info!("SynCord server \"{}\" listening on {addr}", state.config.community_name);
    if state.config.invite_code.is_empty() {
        warn!("invite_code is empty: anyone who can reach this server can register");
    } else {
        info!("invite code for new accounts: {}", state.config.invite_code);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await
        .expect("server error");
}
