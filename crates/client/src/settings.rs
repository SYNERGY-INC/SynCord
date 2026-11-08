//! Client preferences saved between launches.

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
        }
    }
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("syncord").join("settings.toml"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
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
