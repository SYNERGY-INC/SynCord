//! Celerity server: one small binary that stores everything in a single SQLite
//! file and talks to clients over a WebSocket at `/ws`.
//!
//! Usage: `celerity-server [path/to/celerity-server.toml]`
//!
//! Everything it prints also goes to `celerity-server.log` next to the settings
//! file, so problems can be diagnosed even when it runs in the background.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use celerity_server::{AppState, Config, db, router, start_cleanup, start_udp};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::writer::MakeWriterExt;

const CONFIG_FILE: &str = "celerity-server.toml";
const LOG_FILE: &str = "celerity-server.log";
/// Before the rename to Celerity. A server set up back then keeps its files.
const LEGACY_CONFIG_FILE: &str = "syncord-server.toml";
const LEGACY_DATABASE: &str = "syncord.db";

/// A `syncord-server.toml` where `celerity-server.toml` was asked for is renamed to it (or
/// used as it is if it can't be).
fn with_legacy_name(path: PathBuf) -> PathBuf {
    if path.exists() || path.file_name().is_none_or(|n| n != CONFIG_FILE) {
        return path;
    }
    let legacy = path.with_file_name(LEGACY_CONFIG_FILE);
    if !legacy.exists() {
        path
    } else if std::fs::rename(&legacy, &path).is_ok() {
        path
    } else {
        legacy
    }
}
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
/// Bring a settings file written by an older version up to date, keeping the owner's choices.
fn upgrade_settings(path: &Path, mut text: String) -> String {
    let original = text.clone();
    // 25 MB was the old default; files can now be up to 1 GB.
    if text.lines().any(|l| l.trim() == "max_upload_mb = 25") {
        text = text.replace("max_upload_mb = 25\n", "max_upload_mb = 1024\n");
        info!("raised the upload limit to the new default of 1 GB (max_upload_mb)");
    }
    if !text.contains("giphy_api_key") && !text.contains("klipy_api_key") {
        text.push_str(
            "\n# GIF search in the emoji picker: paste a free API key from GIPHY\n\
             # (developers.giphy.com) or KLIPY (partner.klipy.com), then restart the server.\n\
             giphy_api_key = \"\"\n\
             klipy_api_key = \"\"\n",
        );
    }
    if text != original
        && let Err(e) = std::fs::write(path, &text)
    {
        warn!("could not update settings file {}: {e}", path.display());
    }
    text
}

fn load_config(path: &Path) -> Config {
    let mut cfg = match std::fs::read_to_string(path) {
        Ok(text) => {
            let text = upgrade_settings(path, text);
            match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => fail(format!("The settings file {} has a mistake in it: {e}", path.display())),
            }
        }
        Err(_) => {
            let cfg = Config::default();
            let text = format!(
                "# Celerity server settings. Restart the server after editing.\n{}",
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
        // A settings file without `database` used to mean syncord.db.
        if !cfg.database.exists() && dir.join(LEGACY_DATABASE).exists() {
            cfg.database = dir.join(LEGACY_DATABASE);
        }
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.uploads) {
        warn!("could not create the uploads folder {}: {e}", cfg.uploads.display());
    }
    cfg
}

/// Ctrl+C, or (on Linux/macOS) the stop signal that Docker and systemd send.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() {
    // Update downloads use HTTPS; use the same small crypto library as everything else.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| CONFIG_FILE.into());
    let config_path = with_legacy_name(std::path::absolute(&config_path).unwrap_or(config_path));
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

    info!("Celerity Server {} starting (settings: {})", env!("CARGO_PKG_VERSION"), config_path.display());
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
    celerity_server::update::start(state.clone());

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => fail(format!(
            "Port {} is already in use. Celerity Server is probably already running in the background \
             (open http://localhost:{} in a browser to check). If not, another program is using that port: \
             change `bind` in {} to a different port.",
            addr.port(),
            addr.port(),
            config_path.display()
        )),
        Err(e) => fail(format!("Could not listen on {addr}: {e}")),
    };

    info!("Celerity server \"{}\" listening on {addr} (TCP)", state.config.community_name);
    match start_udp(&state, addr).await {
        Ok(port) => info!("voice audio on UDP port {port}"),
        Err(e) => warn!("could not open UDP port for voice ({e}); voice will go over TCP instead"),
    }
    if state.config.invite_code.is_empty() {
        warn!("invite_code is empty: anyone who can reach this server can register");
    } else {
        info!("invite code for new accounts: {}", state.config.invite_code);
    }

    let stopped = async {
        stop_requested().await;
        info!("shutting down");
    };
    let result = match celerity_server::tls::load_or_create(&dir) {
        Ok((tls, fingerprint)) => {
            info!("connections are encrypted (certificate fingerprint {fingerprint})");
            match celerity_server::tls::DualListener::new(listener, tls) {
                Ok(dual) => axum::serve(dual, app).with_graceful_shutdown(stopped).await,
                Err(e) => fail(format!("Could not listen on {addr}: {e}")),
            }
        }
        Err(e) => {
            warn!("encrypted connections are off: {e}");
            axum::serve(listener, app).with_graceful_shutdown(stopped).await
        }
    };
    if let Err(e) = result {
        fail(format!("Server stopped unexpectedly: {e}"));
    }
}
