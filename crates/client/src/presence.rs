//! Your status and activity: going idle when you're away, and noticing which
//! game you're playing (anything run from a Steam, Epic, GOG, Riot or Xbox
//! games folder).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::Task;
use celerity_proto::{Activity, ActivityKind, ClientMsg, Status};

use crate::app::{App, Message};

/// Away this long: idle.
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
/// How often to check for idleness and games.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);

static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// Set while idle because of being away (so the next input says "back").
pub static AUTO_IDLE: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Called for every key press and mouse move in the window (cheap: no redraw).
/// Returns true when this ends an automatic idle.
pub fn input() -> bool {
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    AUTO_IDLE.swap(false, Ordering::Relaxed)
}

/// How long since the person last touched the keyboard or mouse. On Windows this
/// counts the whole computer (so playing a game isn't "away"); elsewhere, the window.
pub fn idle_for() -> Duration {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetTickCount;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        // SAFETY: plain Win32 calls with a correctly sized struct.
        if unsafe { GetLastInputInfo(&mut info) } != 0 {
            let now = unsafe { GetTickCount() };
            return Duration::from_millis(now.wrapping_sub(info.dwTime) as u64);
        }
    }
    let last = LAST_INPUT_MS.load(Ordering::Relaxed);
    if last == 0 {
        LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
        return Duration::ZERO;
    }
    Duration::from_millis(now_ms().saturating_sub(last))
}

/// The game a program belongs to, judging by the folder it's installed in.
pub fn game_from_path(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let lower = norm.to_lowercase();
    // (marker, launchers' own folders to skip)
    const STORES: [(&str, &[&str]); 5] = [
        ("/steamapps/common/", &["steamworks shared", "steam controller configs", "steamvr"]),
        ("/epic games/", &["launcher", "directxredist"]),
        ("/gog galaxy/games/", &[]),
        ("/riot games/", &["riot client"]),
        ("/xboxgames/", &[]),
    ];
    for (marker, skip) in STORES {
        let Some(i) = lower.find(marker) else { continue };
        let start = i + marker.len();
        let name = &norm[start..];
        let name = name.split('/').next().unwrap_or("");
        // Only programs inside the game's folder, not the folder itself.
        if name.is_empty() || !norm[start + name.len()..].starts_with('/') {
            continue;
        }
        if skip.iter().any(|s| name.to_lowercase() == *s) {
            continue;
        }
        return Some(name.to_string());
    }
    None
}

/// Look through running programs for a game (slow-ish: run in the background).
pub fn find_game() -> Option<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let Ok(mut sys) = System::new() else { return None };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    let mut games: Vec<String> =
        sys.processes().values().filter_map(|p| p.exe().and_then(|e| e.to_str()).and_then(game_from_path)).collect();
    games.sort();
    games.dedup();
    games.into_iter().next()
}

impl App {
    /// The status everyone sees: what you chose, or idle if you're away.
    pub(crate) fn effective_status(&self) -> Status {
        if self.settings.status == Status::Online && self.away { Status::Idle } else { self.settings.status }
    }

    /// What you're doing: what you typed, else the game you're playing.
    pub(crate) fn effective_activity(&self) -> Option<Activity> {
        self.settings.custom_activity.clone().or_else(|| {
            self.detected_game
                .clone()
                .filter(|_| self.settings.detect_games)
                .map(|name| Activity { kind: ActivityKind::Playing, name })
        })
    }

    /// Do not disturb: no sounds or pings.
    pub(crate) fn dnd(&self) -> bool {
        self.settings.status == Status::DoNotDisturb
    }

    /// Tell the server, if anything changed since last time.
    pub(crate) fn send_presence(&mut self) {
        let now = (self.effective_status(), self.effective_activity());
        if self.sent_presence.as_ref() == Some(&now) {
            return;
        }
        self.sent_presence = Some(now.clone());
        self.send(ClientMsg::SetPresence { status: now.0, activity: now.1 });
    }

    pub(crate) fn update_presence(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PresenceTick => {
                // Also retry profile pictures that failed to download (loaded ones are skipped).
                let avatars = self.request_avatars();
                let away = idle_for() >= IDLE_AFTER;
                if away != self.away {
                    self.away = away;
                    AUTO_IDLE.store(away, Ordering::Relaxed);
                    self.send_presence();
                }
                if self.settings.detect_games && self.session.is_some() {
                    let game = Task::perform(
                        async { tokio::task::spawn_blocking(find_game).await.ok().flatten() },
                        Message::GameDetected,
                    );
                    return Task::batch([avatars, game]);
                }
                return avatars;
            }
            Message::BackFromIdle => {
                if self.away {
                    self.away = false;
                    self.send_presence();
                }
            }
            Message::GameDetected(game) => {
                if game != self.detected_game {
                    self.detected_game = game;
                    self.send_presence();
                }
            }
            Message::SetStatus(status) => {
                self.settings.status = status;
                self.settings.save();
                self.send_presence();
            }
            Message::ActivityKindPicked(kind) => self.activity_draft.0 = kind,
            Message::ActivityText(text) => self.activity_draft.1 = text.chars().take(64).collect(),
            Message::SaveActivity => {
                let (kind, name) = self.activity_draft.clone();
                self.settings.custom_activity = Activity { kind, name }.sanitized();
                self.settings.save();
                self.send_presence();
                self.close_overlay();
            }
            Message::ClearActivity => {
                self.settings.custom_activity = None;
                self.activity_draft.1.clear();
                self.settings.save();
                self.send_presence();
            }
            Message::ToggleDetectGames(on) => {
                self.settings.detect_games = on;
                if !on {
                    self.detected_game = None;
                }
                self.settings.save();
                self.send_presence();
                if on {
                    return self.update_presence(Message::PresenceTick);
                }
            }
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::game_from_path;

    #[test]
    fn games_by_folder() {
        let g = |p| game_from_path(p);
        assert_eq!(
            g(r"C:\Program Files (x86)\Steam\steamapps\common\Terraria\Terraria.exe").as_deref(),
            Some("Terraria")
        );
        assert_eq!(
            g("D:/SteamLibrary/steamapps/common/Counter-Strike Global Offensive/game/bin/win64/cs2.exe").as_deref(),
            Some("Counter-Strike Global Offensive")
        );
        assert_eq!(
            g(r"C:\Program Files\Epic Games\Fortnite\FortniteGame\Binaries\Win64\x.exe").as_deref(),
            Some("Fortnite")
        );
        assert_eq!(g(r"C:\Riot Games\VALORANT\live\VALORANT.exe").as_deref(), Some("VALORANT"));
        assert_eq!(g("/home/me/.local/share/Steam/steamapps/common/Celeste/Celeste").as_deref(), Some("Celeste"));
        // Launchers and other programs aren't games.
        assert_eq!(g(r"C:\Program Files (x86)\Epic Games\Launcher\Portal\Binaries\Win64\EpicGamesLauncher.exe"), None);
        assert_eq!(g(r"C:\Riot Games\Riot Client\RiotClientServices.exe"), None);
        assert_eq!(g(r"C:\Program Files (x86)\Steam\steam.exe"), None);
        assert_eq!(g(r"C:\Windows\explorer.exe"), None);
    }
}
