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
        }
    }
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("syncord").join("settings.toml"))
}

impl Settings {
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
