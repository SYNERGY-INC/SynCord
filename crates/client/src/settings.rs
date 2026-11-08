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
    pub theme: String,
    /// Multiplier applied to the theme's font size.
    pub text_scale: f32,
    pub show_members: bool,
    pub compact: bool,
    /// Draw the UI on the CPU instead of the GPU. Uses far less memory
    /// (no graphics driver overhead) and is plenty fast for a chat app.
    pub low_memory: bool,

    // Voice
    /// Microphone name; `None` uses the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    /// Sounds quieter than this (dBFS) aren't sent.
    pub input_sensitivity: f32,
    pub noise_suppression: bool,
    /// Volume for each person, keyed by user id.
    pub user_volumes: HashMap<String, f32>,

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
    pub video_muted: bool,
}

/// A GIF starred for quick re-sending (remembered per server).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavoriteGif {
    pub server: String,
    pub attachment: syncord_proto::Attachment,
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
            compact: false,
            low_memory: true,
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            input_sensitivity: -50.0,
            noise_suppression: true,
            user_volumes: HashMap::new(),
            autoplay_gifs: true,
            recent_emoji: Vec::new(),
            favorite_gifs: Vec::new(),
            known_servers: HashMap::new(),
            ignored: HashMap::new(),
            ping_sound: true,
            video_volume: 0.8,
            video_muted: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("syncord").join("settings.toml"))
}

impl Settings {
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
        }
    }

    pub fn remember_emoji(&mut self, emoji: &str) {
        self.recent_emoji.retain(|e| e != emoji);
        self.recent_emoji.insert(0, emoji.to_string());
        self.recent_emoji.truncate(24);
    }

    pub fn favorites_here(&self) -> impl Iterator<Item = &FavoriteGif> {
        self.favorite_gifs.iter().filter(|f| f.server == self.server)
    }

    pub fn user_volume(&self, user: syncord_proto::UserId) -> f32 {
        self.user_volumes.get(&user.to_string()).copied().unwrap_or(1.0)
    }

    pub fn load() -> Self {
        path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
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
