//! Automatic updates for the app: find, download and verify a new release in
//! the background, then install it (after a short countdown popup) and reopen.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use celerity_update::{FileEntry, Manifest, files};

/// Seconds the popup counts down before installing.
pub const COUNTDOWN: u64 = 10;
/// How often a running app looks for updates.
pub const CHECK_EVERY: Duration = Duration::from_secs(3 * 3600);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the update information comes from (overridable for testing).
pub fn manifest_url() -> String {
    std::env::var("CELERITY_UPDATE_URL").unwrap_or_else(|_| celerity_update::MANIFEST_URL.to_string())
}

#[derive(Debug, Clone)]
pub enum Stage {
    /// Looking for (and downloading) an update.
    Checking,
    UpToDate,
    /// Downloaded and verified; installs when the countdown ends (paused during calls).
    Ready {
        version: String,
        notes: String,
        file: PathBuf,
        deadline: Instant,
    },
    /// "Later": installs the next time the app starts.
    Postponed {
        version: String,
    },
    /// Can't be installed automatically (system package): offer the download page.
    Manual {
        version: String,
        page: String,
        dismissed: bool,
    },
    Installing(String),
    Failed(String),
}

/// How this copy of the app can be updated.
enum Method {
    WindowsInstaller,
    MacBundle(PathBuf),
    LinuxBinary(PathBuf),
    Manual,
}

fn method() -> Method {
    let Ok(exe) = std::env::current_exe() else { return Method::Manual };
    if cfg!(windows) {
        return Method::WindowsInstaller;
    }
    if cfg!(target_os = "macos") {
        // .../Celerity.app/Contents/MacOS/Celerity
        return match exe.ancestors().nth(3) {
            Some(app) if app.extension().is_some_and(|e| e == "app") && writable_dir(app.parent()) => {
                Method::MacBundle(app.to_path_buf())
            }
            _ => Method::Manual,
        };
    }
    // Linux: a copy in a folder we can write to (the portable download) updates itself;
    // the system package (/usr/bin) needs the user's password, so it's done by hand.
    if writable_dir(exe.parent()) { Method::LinuxBinary(exe) } else { Method::Manual }
}

fn writable_dir(dir: Option<&Path>) -> bool {
    let Some(dir) = dir else { return false };
    let probe = dir.join(".celerity-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Which manifest file this copy of the app wants.
fn wanted_file(m: &Manifest) -> Option<(&FileEntry, bool)> {
    let auto = |key: &str| m.files.get(key).map(|f| (f, true));
    match method() {
        Method::WindowsInstaller => auto(files::WINDOWS_APP),
        Method::MacBundle(_) => auto(files::MACOS_APP).or_else(|| auto(files::MACOS_LEGACY)),
        Method::LinuxBinary(_) => auto(files::LINUX_APP),
        Method::Manual => m.files.get(files::LINUX_DEB).or_else(|| m.files.get(files::MACOS_APP)).map(|f| (f, false)),
    }
}

fn download_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("celerity").join("updates")
}

/// Outcome of a background check-and-download.
#[derive(Debug, Clone)]
pub enum Found {
    UpToDate,
    Ready { version: String, notes: String, file: PathBuf },
    Manual { version: String, page: String },
}

/// Look for a newer release; if there is one, download and verify it.
pub async fn check_and_download() -> Result<Found, String> {
    let http = celerity_update::client();
    let manifest = celerity_update::fetch_manifest(&http, &manifest_url()).await?;
    if !celerity_update::is_newer(&manifest.version, VERSION) {
        return Ok(Found::UpToDate);
    }
    let Some((entry, automatic)) = wanted_file(&manifest) else { return Ok(Found::UpToDate) };
    if !automatic {
        let page = if manifest.page.is_empty() { entry.url.clone() } else { manifest.page.clone() };
        return Ok(Found::Manual { version: manifest.version, page });
    }

    let dir = download_dir();
    let file = dir.join(&entry.name);
    // Reuse an earlier download if it's still intact.
    let cached =
        std::fs::read(&file).ok().filter(|b| celerity_update::verify(b, entry, &celerity_update::PUBLIC_KEY).is_ok());
    if cached.is_none() {
        let bytes = celerity_update::download(&http, entry, &celerity_update::PUBLIC_KEY).await?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't save the update: {e}"))?;
        std::fs::write(&file, bytes).map_err(|e| format!("couldn't save the update: {e}"))?;
    }
    Ok(Found::Ready { version: manifest.version, notes: manifest.notes, file })
}

/// Install a downloaded update and start the new version. The caller then exits.
pub fn install(file: &Path) -> Result<(), String> {
    match method() {
        Method::WindowsInstaller => {
            // The installer closes this app, installs quietly, and reopens Celerity (/RELAUNCH).
            std::process::Command::new(file)
                .args(["/S", "/RELAUNCH"])
                .spawn()
                .map_err(|e| format!("couldn't start the installer: {e}"))?;
            Ok(())
        }
        Method::MacBundle(app) => install_mac(file, &app),
        Method::LinuxBinary(exe) => install_linux(file, &exe),
        Method::Manual => Err("this copy of Celerity has to be updated by hand".into()),
    }
}

fn run(cmd: &mut std::process::Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn install_mac(dmg: &Path, app: &Path) -> Result<(), String> {
    use std::process::Command;
    let parent = app.parent().ok_or("app has no folder")?;
    let mount = std::env::temp_dir().join(format!("celerity-update-{}", std::process::id()));
    std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    run(Command::new("hdiutil")
        .arg("attach")
        .arg("-nobrowse")
        .arg("-readonly")
        .arg("-mountpoint")
        .arg(&mount)
        .arg(dmg))?;
    let staged = parent.join(".Celerity-update.app");
    let _ = std::fs::remove_dir_all(&staged);
    // (Disk images made for SynCord's updater name the app SynCord.app.)
    let source = ["Celerity.app", "SynCord.app"].map(|n| mount.join(n)).into_iter().find(|p| p.exists());
    let copied = match source {
        Some(source) => run(Command::new("ditto").arg(source).arg(&staged)),
        None => Err("the update doesn't contain the app".to_string()),
    };
    let _ = run(Command::new("hdiutil").arg("detach").arg(&mount).arg("-quiet"));
    copied?;
    // A copy still called SynCord.app (updated from SynCord) becomes Celerity.app.
    let renamed = parent.join("Celerity.app");
    let target = if app.file_name().is_some_and(|n| n == "SynCord.app") && !renamed.exists() { &renamed } else { app };
    // Swap the new app in; keep the old one until the swap has worked.
    let old = parent.join(".Celerity-old.app");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(app, &old).map_err(|e| format!("couldn't replace the app: {e}"))?;
    if let Err(e) = std::fs::rename(&staged, target) {
        let _ = std::fs::rename(&old, app);
        return Err(format!("couldn't replace the app: {e}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    // The new copy waits for this one to exit instead of handing over to it.
    run(Command::new("open").arg("-n").arg("--env").arg(format!("{}=1", crate::tray::REPLACING_ENV)).arg(target))
}

fn install_linux(archive: &Path, exe: &Path) -> Result<(), String> {
    use std::process::Command;
    let work = std::env::temp_dir().join(format!("celerity-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    run(Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&work))?;
    let new_binary = std::fs::read_dir(&work)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path().join("celerity"))
        .find(|p| p.is_file())
        .ok_or("the update archive doesn't contain the app")?;
    // Copy next to the running program, then rename over it (allowed while it runs).
    let staged = exe.with_file_name(".celerity-update");
    std::fs::copy(&new_binary, &staged).map_err(|e| format!("couldn't install the update: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&staged, exe).map_err(|e| format!("couldn't install the update: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    Command::new(exe)
        .env(crate::tray::REPLACING_ENV, "1")
        .spawn()
        .map_err(|e| format!("couldn't restart Celerity: {e}"))?;
    Ok(())
}

/// Open a web page in the default browser.
pub fn open_in_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd.arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_right_file() {
        let entry = |name: &str| FileEntry {
            name: name.into(),
            url: format!("https://x/{name}"),
            sha256: String::new(),
            size: 0,
            signature: None,
        };
        let m = Manifest {
            version: "9.9.9".into(),
            notes: String::new(),
            page: "https://x/page".into(),
            files: [
                (files::WINDOWS_APP.to_string(), entry("a.exe")),
                (files::MACOS_APP.to_string(), entry("a.dmg")),
                (files::LINUX_APP.to_string(), entry("a.tar.gz")),
                (files::LINUX_DEB.to_string(), entry("a.deb")),
            ]
            .into(),
        };
        let (f, _) = wanted_file(&m).expect("a file for this platform");
        if cfg!(windows) {
            assert_eq!(f.name, "a.exe");
        } else if cfg!(target_os = "linux") {
            // Test binaries live in target/, which is writable, so they'd update themselves.
            assert_eq!(f.name, "a.tar.gz");
        }
    }
}
