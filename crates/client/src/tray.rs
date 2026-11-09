//! Keeping Celerity running in the background: the tray icon (Windows and
//! macOS), and making a second launch bring back the running copy instead of
//! opening another one.
//!
//! Events from the tray and from second launches arrive on one channel, which
//! the app listens to as a subscription, so nothing is polled while idle.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};

use futures::Stream;
use tokio::sync::mpsc;

/// Something outside the window wants the app's attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// Show the window (tray click, "Open Celerity", or Celerity started again).
    Show,
    /// Open a `celerity://` link (a notification was clicked, or a link was opened elsewhere).
    Open(String),
    /// "Quit Celerity" in the tray menu (there's no tray on Linux).
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    Quit,
}

type Channel = (mpsc::UnboundedSender<Wake>, Mutex<Option<mpsc::UnboundedReceiver<Wake>>>);

fn channel() -> &'static Channel {
    static CHANNEL: OnceLock<Channel> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Mutex::new(Some(rx)))
    })
}

fn wake(w: Wake) {
    let _ = channel().0.send(w);
}

/// Show the window and open `link` (a `celerity://` link).
pub fn open_link(link: String) {
    wake(Wake::Open(link));
}

/// A `celerity://` link Celerity was started with (from a browser or the system), if any.
pub fn link_argument() -> Option<String> {
    std::env::args().skip(1).find(|a| a.starts_with("celerity://"))
}

/// The app's subscription to tray and second-launch events.
pub fn events() -> impl Stream<Item = Wake> {
    let rx = channel().1.lock().unwrap().take();
    futures::stream::unfold(rx, |rx| async move {
        let mut rx = rx?;
        let w = rx.recv().await?;
        Some((w, Some(rx)))
    })
}

// ---- One copy at a time

fn instance_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("celerity")
}

/// Set by the updater when it starts the new version: wait for the old copy to exit.
pub const REPLACING_ENV: &str = "CELERITY_REPLACING";
/// What SynCord's updater sets when it starts Celerity.
const LEGACY_REPLACING_ENV: &str = "SYNCORD_REPLACING";

/// Make sure only one copy runs (per user). Returns false if another copy was
/// asked to show itself, in which case this one should quit.
pub fn claim_single_instance() -> bool {
    let dir = instance_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(lock) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("instance.lock"))
    else {
        return true; // can't tell: just run
    };
    if std::env::var_os(LEGACY_REPLACING_ENV).is_some() {
        // SynCord (the old name) is updating to this: wait for it to close.
        let old = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("syncord").join("instance.lock");
        if let Ok(old) = std::fs::OpenOptions::new().write(true).open(old) {
            for _ in 0..100 {
                if old.try_lock().is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    let replacing = std::env::var_os(REPLACING_ENV).is_some();
    let mut tries = if replacing { 100 } else { 1 };
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(_) if tries > 1 => {
                tries -= 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => {
                // Another copy is running: ask it to show itself.
                if let Ok(port) = std::fs::read_to_string(dir.join("instance.port"))
                    && let Ok(port) = port.trim().parse::<u16>()
                    && let Ok(mut s) = TcpStream::connect(("127.0.0.1", port))
                {
                    let _ = s.write_all(b"show");
                    // Newer copies also take a link to open; older ones stop reading after "show".
                    if let Some(link) = link_argument() {
                        let _ = s.write_all(format!("\nopen {link}").as_bytes());
                    }
                    return false;
                }
                return true; // it didn't answer: run anyway
            }
        }
    }
    // Keep the lock for as long as this copy runs.
    std::mem::forget(lock);
    if let Ok(listener) = TcpListener::bind(("127.0.0.1", 0))
        && let Ok(addr) = listener.local_addr()
    {
        let _ = std::fs::write(dir.join("instance.port"), addr.port().to_string());
        let _ = std::thread::Builder::new().name("celerity-instance".into()).spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut buf = [0u8; 4];
                if conn.read_exact(&mut buf).is_ok() && &buf == b"show" {
                    let _ = conn.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                    let mut rest = String::new();
                    let _ = conn.take(4096).read_to_string(&mut rest);
                    match rest.trim().strip_prefix("open ") {
                        Some(link) if link.starts_with("celerity://") => wake(Wake::Open(link.to_string())),
                        _ => wake(Wake::Show),
                    }
                }
            }
        });
    }
    true
}

// ---- Tray icon

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{Wake, wake};

    pub struct Tray {
        _icon: TrayIcon,
    }

    pub const SUPPORTED: bool = true;

    pub fn create() -> Result<Tray, String> {
        let open = MenuItem::new("Open Celerity", true, None);
        let quit = MenuItem::new("Quit Celerity", true, None);
        let menu = Menu::new();
        menu.append(&open).map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;
        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                wake(Wake::Show);
            } else if e.id == quit_id {
                wake(Wake::Quit);
            }
        }));
        TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                wake(Wake::Show);
            }
        }));
        let icon =
            tray_icon::Icon::from_rgba(include_bytes!("../../../packaging/icons/celerity-64.rgba").to_vec(), 64, 64)
                .map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Celerity")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Tray { _icon: tray })
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    /// Linux desktops need extra GTK libraries for tray icons, so there the X button just quits.
    pub struct Tray;

    pub const SUPPORTED: bool = false;

    pub fn create() -> Result<Tray, String> {
        Err("tray icons aren't supported on this system".into())
    }
}

pub use imp::{SUPPORTED, Tray, create};
