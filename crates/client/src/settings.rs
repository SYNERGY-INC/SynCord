//! Client preferences saved between launches.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: String,
    pub username: String,
    /// Session token for automatic login. Cleared on log out.
    pub token: Option<String>,
    /// More servers you're signed in to (shown in the server rail), in rail order without
    /// the one above, which sits at `server_position`.
    pub other_servers: Vec<SavedServer>,
    pub server_position: usize,
    pub theme: String,
    /// Multiplier applied to the theme's font size.
    pub text_scale: f32,
    pub show_members: bool,
    /// UI font family ("" for the built-in Inter). Applied when Celerity starts.
    pub ui_font: String,
    /// Font file imported by the user (kept in the settings folder).
    pub font_file: Option<PathBuf>,
    pub compact: bool,
    /// Draw the UI on the CPU instead of the GPU. Uses less memory (no graphics
    /// driver), but scrolling and GIFs aren't as smooth.
    pub low_memory: bool,
    /// Set once `low_memory` was decided with 0.6's default (the GPU). Apps before 0.6
    /// defaulted to the CPU, which made scrolling laggy, so they're switched over once.
    #[serde(default)]
    pub renderer_v06: bool,

    // Voice
    /// Microphone name; `None` uses the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    /// Sounds quieter than this (dBFS) aren't sent.
    pub input_sensitivity: f32,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    /// Screen sharing and camera quality (labels of celerity_stream's presets).
    pub screen_quality: String,
    pub camera_quality: String,
    /// Share your computer's (or the window's app's) sound with your screen.
    pub share_audio: bool,
    /// How loud each person's screen share sound plays (by user id, 1.0 = as sent).
    pub stream_volumes: HashMap<String, f32>,
    /// Push to talk instead of sending when you speak, with this key (see voice::ptt).
    pub push_to_talk: bool,
    pub ptt_key: String,
    /// Volume for each person, keyed by user id.
    pub user_volumes: HashMap<String, f32>,

    // Sounds
    /// Play a sound for new direct messages from others.
    pub dm_sound: bool,
    /// Play a sound for new messages from others in server channels.
    pub server_sound: bool,
    /// Pop up a desktop notification for direct messages and @mentions while Celerity
    /// isn't in front...
    pub desktop_notifications: bool,
    /// ...or for every message.
    pub notify_all: bool,
    /// Muted servers (by address) and channels (per server): no sounds, notifications or
    /// unread dots, except when someone @mentions you.
    pub muted_servers: Vec<String>,
    pub muted_channels: HashMap<String, Vec<i64>>,
    /// Play a sound when people join or leave your voice channel.
    pub voice_sounds: bool,
    /// Flash the taskbar icon for every new message (mentions always flash) while Celerity isn't focused.
    pub flash_taskbar: bool,
    /// Volume of notification sounds (0.0 to 1.0).
    pub sound_volume: f32,

    // Pictures and emoji
    /// Play GIFs all the time (otherwise only while the mouse is over them).
    pub autoplay_gifs: bool,
    /// Most recently used emoji, newest first.
    pub recent_emoji: Vec<String>,
    pub favorite_gifs: Vec<FavoriteGif>,

    /// Certificate fingerprints of servers we've connected to (host:port -> fingerprint).
    pub known_servers: HashMap<String, String>,

    /// People whose messages are folded away, per server (only on this computer).
    pub ignored: HashMap<String, Vec<i64>>,
    /// Play a sound when someone mentions you.
    pub ping_sound: bool,
    pub video_volume: f32,
    /// The X button hides Celerity to the tray (Windows and macOS) instead of quitting.
    pub close_to_tray: bool,

    // Status
    pub status: celerity_proto::Status,
    /// "Playing ..." typed by you (wins over a detected game).
    pub custom_activity: Option<celerity_proto::Activity>,
    /// Show the game you're playing (found from where it's installed).
    pub detect_games: bool,
    pub video_muted: bool,
    /// Camera for video calls, by name (None: the first one).
    pub camera: Option<String>,
    /// Websites whose links open without asking first.
    pub trusted_sites: Vec<String>,
}

/// A GIF starred for quick re-sending (remembered per server).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavoriteGif {
    pub server: String,
    pub attachment: celerity_proto::Attachment,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: String::new(),
            username: String::new(),
            token: None,
            theme: "Classic Dark".into(),
            text_scale: 1.0,
            show_members: true,
            ui_font: String::new(),
            font_file: None,
            compact: false,
            low_memory: false,
            renderer_v06: true,
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            input_sensitivity: -50.0,
            noise_suppression: true,
            echo_cancellation: true,
            screen_quality: celerity_stream::ScreenQuality::Res1440p60.label().into(),
            camera_quality: celerity_stream::CameraQuality::Res1080p.label().into(),
            // On Linux the computer's sound includes the call itself, so it starts off there.
            share_audio: !cfg!(all(unix, not(target_os = "macos"))),
            stream_volumes: HashMap::new(),
            push_to_talk: false,
            ptt_key: "Backquote".into(),
            user_volumes: HashMap::new(),
            dm_sound: true,
            server_sound: true,
            desktop_notifications: true,
            notify_all: false,
            muted_servers: Vec::new(),
            muted_channels: HashMap::new(),
            voice_sounds: true,
            flash_taskbar: true,
            sound_volume: 0.6,
            autoplay_gifs: true,
            recent_emoji: Vec::new(),
            favorite_gifs: Vec::new(),
            known_servers: HashMap::new(),
            other_servers: Vec::new(),
            server_position: 0,
            ignored: HashMap::new(),
            ping_sound: true,
            video_volume: 0.8,
            close_to_tray: true,
            status: celerity_proto::Status::Online,
            custom_activity: None,
            detect_games: true,
            camera: None,
            trusted_sites: Vec::new(),
            video_muted: false,
        }
    }
}

/// A server you're signed in to besides the open one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedServer {
    pub server: String,
    pub username: String,
    pub token: Option<String>,
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("celerity").join("settings.toml"))
}

/// Celerity used to be called SynCord: bring its settings, themes, fonts and trusted
/// servers along the first time Celerity starts.
pub fn adopt_legacy_folder() {
    let Some(config) = dirs::config_dir() else { return };
    let (old, new) = (config.join("syncord"), config.join("celerity"));
    if new.exists() || !old.is_dir() {
        return;
    }
    if std::fs::rename(&old, &new).is_err() {
        // Somewhere a rename can't reach: copy instead and leave the old folder be.
        let _ = copy_dir(&old, &new);
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl Settings {
    /// The server (or this channel on it) is muted.
    pub fn is_muted(&self, server: &str, channel: i64) -> bool {
        let key = crate::net::server_key(server);
        self.muted_servers.contains(&key) || self.muted_channels.get(&key).is_some_and(|c| c.contains(&channel))
    }

    pub fn channel_muted(&self, server: &str, channel: i64) -> bool {
        self.muted_channels.get(&crate::net::server_key(server)).is_some_and(|c| c.contains(&channel))
    }

    pub fn set_server_muted(&mut self, server: &str, muted: bool) {
        let key = crate::net::server_key(server);
        self.muted_servers.retain(|k| *k != key);
        if muted {
            self.muted_servers.push(key);
        }
    }

    pub fn set_channel_muted(&mut self, server: &str, channel: i64, muted: bool) {
        let list = self.muted_channels.entry(crate::net::server_key(server)).or_default();
        list.retain(|c| *c != channel);
        if muted {
            list.push(channel);
        }
    }

    /// People ignored on this server.
    pub fn ignored_here(&self, server: &str) -> &[i64] {
        self.ignored.get(&crate::net::server_key(server)).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn set_ignored(&mut self, server: &str, user: i64, ignored: bool) {
        let list = self.ignored.entry(crate::net::server_key(server)).or_default();
        list.retain(|u| *u != user);
        if ignored {
            list.push(user);
        }
    }

    pub fn voice_config(&self) -> crate::voice::VoiceConfig {
        crate::voice::VoiceConfig {
            input_device: self.input_device.clone(),
            output_device: self.output_device.clone(),
            input_gain: self.input_gain,
            output_volume: self.output_volume,
            sensitivity_db: self.input_sensitivity,
            noise_suppression: self.noise_suppression,
            echo_cancellation: self.echo_cancellation,
            push_to_talk: (self.push_to_talk && !self.ptt_key.is_empty()).then(|| self.ptt_key.clone()),
        }
    }

    /// Play a notification sound on the chosen speakers, at the chosen volume.
    pub fn play(&self, sound: crate::sounds::Sound) {
        crate::sounds::play(sound, self.output_device.clone(), self.sound_volume);
    }

    pub fn remember_emoji(&mut self, emoji: &str) {
        self.recent_emoji.retain(|e| e != emoji);
        self.recent_emoji.insert(0, emoji.to_string());
        self.recent_emoji.truncate(24);
    }

    pub fn favorites_here(&self) -> impl Iterator<Item = &FavoriteGif> {
        self.favorite_gifs.iter().filter(|f| f.server == self.server)
    }

    pub fn screen_quality(&self) -> celerity_stream::ScreenQuality {
        celerity_stream::ScreenQuality::ALL.into_iter().find(|q| q.label() == self.screen_quality).unwrap_or_default()
    }

    pub fn camera_quality(&self) -> celerity_stream::CameraQuality {
        celerity_stream::CameraQuality::ALL.into_iter().find(|q| q.label() == self.camera_quality).unwrap_or_default()
    }

    pub fn stream_volume(&self, user: celerity_proto::UserId) -> f32 {
        self.stream_volumes.get(&user.to_string()).copied().unwrap_or(1.0)
    }

    pub fn user_volume(&self, user: celerity_proto::UserId) -> f32 {
        self.user_volumes.get(&user.to_string()).copied().unwrap_or(1.0)
    }

    pub fn load() -> Self {
        let mut s: Self = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        if !s.renderer_v06 {
            // Older versions drew on the CPU by default: smooth scrolling needs the GPU.
            s.low_memory = false;
            s.renderer_v06 = true;
            s.save();
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&p, text);
            #[cfg(unix)]
            {
                // The file holds a login token, so keep it private.
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

#[cfg(test)]
mod renderer_tests {
    use super::Settings;

    #[test]
    fn older_settings_switch_to_the_gpu_once() {
        // A 0.5 settings file: low memory on (its default), no marker.
        let old: Settings = toml::from_str("low_memory = true").unwrap();
        assert!(!old.renderer_v06 && old.low_memory);
        // Someone who turns it on again in 0.6 keeps it.
        let chosen: Settings = toml::from_str("low_memory = true\nrenderer_v06 = true").unwrap();
        assert!(chosen.low_memory);
        assert!(!Settings::default().low_memory && Settings::default().renderer_v06);
    }
}
