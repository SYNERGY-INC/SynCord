//! SynCord server: one small binary that stores everything in a single SQLite
//! file and talks to clients over a WebSocket at `/ws`.
//!
//! Usage: `syncord-server [path/to/syncord-server.toml]`
//!
//! Everything it prints also goes to `syncord-server.log` next to the settings
//! file, so problems can be diagnosed even when it runs in the background.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use syncord_server::{AppState, Config, db, router, start_cleanup, start_udp};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::writer::MakeWriterExt;

const CONFIG_FILE: &str = "syncord-server.toml";
const LOG_FILE: &str = "syncord-server.log";
/// Start a fresh log when the old one gets bigger than this.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Report a fatal problem and exit. When someone started the server by
/// double-clicking it, keep the window open so they can read the message.
fn fail(message: impl std::fmt::Display) -> ! {
    error!("{message}");
    if std::io::stdin().is_terminal() && cfg!(windows) {
        eprintln!("\nPress Enter to close this window.");
        let _ = std::io::stdin().read(&mut [0u8]);
    }
    std::process::exit(1);
}

fn open_log(dir: &Path) -> Option<File> {
    let path = dir.join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{LOG_FILE}.old")));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Load the settings file, creating it with defaults (and a fresh invite code)
/// if it doesn't exist yet.
///
/// A relative `database` path is taken relative to the settings file's folder,
/// so the server finds its data no matter which folder it was started from.
fn load_config(path: &Path) -> Config {
    let mut cfg = match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => fail(format!("The settings file {} has a mistake in it: {e}", path.display())),
        },
        Err(_) => {
            let cfg = Config::default();
            let text = format!(
                "# SynCord server settings. Restart the server after editing.\n{}",
                toml::to_string_pretty(&cfg).unwrap()
            );
            match std::fs::write(path, text) {
                Ok(()) => info!("created settings file {}", path.display()),
                Err(e) => warn!(
                    "could not create settings file {} ({e}); using a temporary invite code that will change on restart",
                    path.display()
                ),
            }
            cfg
        }
    };
    if let Some(dir) = path.parent() {
        for p in [&mut cfg.database, &mut cfg.uploads] {
            if p.is_relative() {
                *p = dir.join(&*p);
            }
        }
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.uploads) {
        warn!("could not create the uploads folder {}: {e}", cfg.uploads.display());
    }
    cfg
}

#[tokio::main]
async fn main() {
    // Update downloads use HTTPS; use the same small crypto library as everything else.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| CONFIG_FILE.into());
    let config_path = std::path::absolute(&config_path).unwrap_or(config_path);
    let dir = config_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let _ = std::fs::create_dir_all(&dir);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match open_log(&dir) {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr.and(Arc::new(file)))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }

    info!("SynCord Server {} starting (settings: {})", env!("CARGO_PKG_VERSION"), config_path.display());
    let config = load_config(&config_path);

    let db = match db::Db::open(&config.database) {
        Ok(db) => db,
        Err(e) => fail(format!(
            "Could not open the database {}: {e}. Check the folder exists and this program may write to it.",
            config.database.display()
        )),
    };

    let addr: SocketAddr = match config.bind.parse() {
        Ok(a) => a,
        Err(e) => fail(format!(
            "The `bind` setting {:?} isn't a valid address ({e}). It should look like \"0.0.0.0:7878\".",
            config.bind
        )),
    };

    let state = AppState::new(config, db);
    let app = router(state.clone());
    start_cleanup(state.clone());
    syncord_server::update::start(state.clone());

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => fail(format!(
            "Port {} is already in use. SynCord Server is probably already running in the background \
             (open http://localhost:{} in a browser to check). If not, another program is using that port: \
             change `bind` in {} to a different port.",
            addr.port(),
            addr.port(),
            config_path.display()
        )),
        Err(e) => fail(format!("Could not listen on {addr}: {e}")),
    };

    info!("SynCord server \"{}\" listening on {addr} (TCP)", state.config.community_name);
    match start_udp(&state, addr).await {
        Ok(port) => info!("voice audio on UDP port {port}"),
        Err(e) => warn!("could not open UDP port for voice ({e}); voice will go over TCP instead"),
    }
    if state.config.invite_code.is_empty() {
        warn!("invite_code is empty: anyone who can reach this server can register");
    } else {
        info!("invite code for new accounts: {}", state.config.invite_code);
    }

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await
    {
        fail(format!("Server stopped unexpectedly: {e}"));
    }
}
