//! The server keeps itself up to date. Windows installs (made by the server
//! installer) download and verify new versions, then install them when nobody
//! is connected. Elsewhere the server just logs that an update is available.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use syncord_update::files;
use tracing::{info, warn};

use crate::AppState;

const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// After this long waiting for an empty server, install anyway (if nobody's in a call).
const MAX_WAIT: Duration = Duration::from_secs(24 * 3600);

/// Installed by the Windows server installer (which puts its scripts next to the program)?
fn windows_install() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join("server-setup.ps1").is_file().then(|| dir.to_path_buf())
}

pub fn start(state: Arc<AppState>) {
    if !state.config.auto_update {
        info!("automatic updates are turned off (auto_update = false)");
        return;
    }
    tokio::spawn(async move {
        // Let the server settle before the first check.
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            if let Err(e) = check_once(&state).await {
                warn!("update check failed: {e}");
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

async fn check_once(state: &Arc<AppState>) -> Result<(), String> {
    let http = syncord_update::client();
    let manifest = syncord_update::fetch_manifest(&http, &state.config.update_url).await?;
    let current = env!("CARGO_PKG_VERSION");
    if !syncord_update::is_newer(&manifest.version, current) {
        return Ok(());
    }
    if windows_install().is_none() {
        if std::path::Path::new("/.dockerenv").exists() {
            info!(
                "SynCord Server {} is available (you have {current}). Pull the new image to update \
                 (in CasaOS: open the app's settings and update it).",
                manifest.version
            );
        } else {
            info!(
                "SynCord Server {} is available (you have {current}). Install it with the server package for your system: {}",
                manifest.version, manifest.page
            );
        }
        return Ok(());
    }
    let entry = manifest.files.get(files::WINDOWS_SERVER).ok_or("the release has no Windows server installer")?;
    info!("downloading SynCord Server {}", manifest.version);
    let bytes = syncord_update::download(&http, entry, &syncord_update::PUBLIC_KEY).await?;
    let dir = state.config.database.parent().map(|p| p.join("updates")).unwrap_or_else(|| PathBuf::from("updates"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let installer = dir.join(&entry.name);
    std::fs::write(&installer, bytes).map_err(|e| e.to_string())?;

    // Wait for a quiet moment: nobody connected, or after a day, at least nobody in a call.
    let waiting_since = Instant::now();
    loop {
        let nobody_online = state.online.lock().unwrap().is_empty();
        let nobody_in_voice = state.voice.states().is_empty();
        if nobody_online || (waiting_since.elapsed() > MAX_WAIT && nobody_in_voice) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    info!("installing SynCord Server {}; the server will restart", manifest.version);
    launch_installer(&installer)
}

/// Run the installer as its own scheduled task: it stops this server, so it
/// mustn't be one of this server's child processes.
fn launch_installer(installer: &std::path::Path) -> Result<(), String> {
    use std::process::Command;
    let task = "SynCord Server Update";
    let command = format!("\"{}\" /S", installer.display());
    let created = Command::new("schtasks")
        .args(["/Create", "/F", "/TN", task, "/SC", "ONCE", "/ST", "00:00", "/RU", "SYSTEM", "/RL", "HIGHEST", "/TR"])
        .arg(&command)
        .output()
        .map_err(|e| e.to_string())?;
    if !created.status.success() {
        return Err(format!("couldn't schedule the update: {}", String::from_utf8_lossy(&created.stdout).trim()));
    }
    let run = Command::new("schtasks").args(["/Run", "/TN", task]).output().map_err(|e| e.to_string())?;
    if !run.status.success() {
        return Err(format!("couldn't start the update: {}", String::from_utf8_lossy(&run.stdout).trim()));
    }
    Ok(())
}
