//! App themes. Built-in themes live here; users can add their own by dropping
//! `.toml` files into the themes folder (see [`themes_dir`]).

use std::path::PathBuf;

use iced::{Color, theme};
use serde::{Deserialize, Serialize};

/// A colour stored as `"#RRGGBB"` (or `"#RRGGBBAA"`) in theme files.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hex(pub Color);

impl Hex {
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim().trim_start_matches('#');
        let v = u32::from_str_radix(s, 16).ok()?;
        match s.len() {
            6 => Some(rgb(v)),
            8 => Some(Color::from_rgba8((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, (v & 0xFF) as f32 / 255.0)),
            _ => None,
        }
    }
}

impl Serialize for Hex {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let [r, g, b, a] = self.0.into_rgba8();
        if a == 255 {
            s.serialize_str(&format!("#{r:02X}{g:02X}{b:02X}"))
        } else {
            s.serialize_str(&format!("#{r:02X}{g:02X}{b:02X}{a:02X}"))
        }
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hex::parse(&s).map(Hex).ok_or_else(|| serde::de::Error::custom(format!("invalid colour {s:?}")))
    }
}

pub fn rgb(v: u32) -> Color {
    Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub fn to_u32(c: Color) -> u32 {
    let [r, g, b, _] = c.into_rgba8();
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Every colour and size the UI uses. Field names describe where they appear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppTheme {
    pub name: String,
    /// Far-left server bar.
    pub rail: Hex,
    /// Channel list and member list.
    pub sidebar: Hex,
    /// Your user panel at the bottom-left.
    pub user_panel: Hex,
    /// Main chat area.
    pub chat: Hex,
    /// Message box, popups and cards.
    pub floating: Hex,
    /// Hovered rows.
    pub hover: Hex,
    /// Selected channel.
    pub selected: Hex,
    /// Body text.
    pub text: Hex,
    /// Timestamps, hints, channel names.
    pub muted: Hex,
    /// Headings and names.
    pub header: Hex,
    /// Buttons, links, highlights.
    pub accent: Hex,
    pub online: Hex,
    pub danger: Hex,
    /// Base font size in points.
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Corner roundness of buttons, inputs and cards.
    #[serde(default = "default_radius")]
    pub radius: f32,
}

fn default_font_size() -> f32 {
    15.0
}
fn default_radius() -> f32 {
    8.0
}

#[allow(clippy::too_many_arguments)]
fn make(
    name: &str,
    rail: u32,
    sidebar: u32,
    user_panel: u32,
    chat: u32,
    floating: u32,
    hover: u32,
    selected: u32,
    text: u32,
    muted: u32,
    header: u32,
    accent: u32,
) -> AppTheme {
    AppTheme {
        name: name.into(),
        rail: Hex(rgb(rail)),
        sidebar: Hex(rgb(sidebar)),
        user_panel: Hex(rgb(user_panel)),
        chat: Hex(rgb(chat)),
        floating: Hex(rgb(floating)),
        hover: Hex(rgb(hover)),
        selected: Hex(rgb(selected)),
        text: Hex(rgb(text)),
        muted: Hex(rgb(muted)),
        header: Hex(rgb(header)),
        accent: Hex(rgb(accent)),
        online: Hex(rgb(0x23A55A)),
        danger: Hex(rgb(0xF23F43)),
        font_size: default_font_size(),
        radius: default_radius(),
    }
}

pub fn builtin() -> Vec<AppTheme> {
    vec![
        make(
            "Classic Dark",
            0x1E1F22,
            0x2B2D31,
            0x232428,
            0x313338,
            0x383A40,
            0x35373C,
            0x404249,
            0xDBDEE1,
            0x949BA4,
            0xF2F3F5,
            0x5865F2,
        ),
        make(
            "Light", 0xE3E5E8, 0xF2F3F5, 0xEBEDEF, 0xFFFFFF, 0xEBEDEF, 0xE8E9EB, 0xD7D9DC, 0x313338, 0x5C5E66,
            0x060607, 0x5865F2,
        ),
        make(
            "Midnight", 0x000000, 0x0B0B0D, 0x050506, 0x000000, 0x141417, 0x15151A, 0x1E1E24, 0xD6D7DA, 0x80848E,
            0xFFFFFF, 0x7B83FF,
        ),
        make(
            "Ocean", 0x0B1622, 0x10202F, 0x0C1A27, 0x14283A, 0x1B3349, 0x1A3147, 0x234260, 0xD3E3F0, 0x7F9BB3,
            0xF0F7FF, 0x2BB3C0,
        ),
        make(
            "Forest", 0x111A14, 0x18241C, 0x131D16, 0x1D2B21, 0x26372A, 0x243428, 0x2E4433, 0xD8E6DA, 0x86A08C,
            0xF2FFF4, 0x4CAF6A,
        ),
        make(
            "Sakura", 0x2A1A22, 0x35222C, 0x2E1D26, 0x3D2833, 0x48303D, 0x46303B, 0x573A49, 0xF3DDE7, 0xB792A4,
            0xFFF0F6, 0xF47FB0,
        ),
        make(
            "Ember", 0x1A1210, 0x241915, 0x1E1411, 0x2C1F1A, 0x382821, 0x35261F, 0x45312A, 0xEEDDD5, 0xA88E83,
            0xFFF3EE, 0xFF7A3D,
        ),
    ]
}

/// Folder where custom theme files live.
pub fn themes_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("syncord").join("themes"))
}

/// Built-in themes followed by any valid user themes. Also writes an example
/// theme file the first time so people have something to copy.
pub fn load_all() -> (Vec<AppTheme>, Vec<String>) {
    let mut themes = builtin();
    let mut errors = Vec::new();
    let Some(dir) = themes_dir() else { return (themes, errors) };

    if !dir.exists() && std::fs::create_dir_all(&dir).is_ok() {
        let mut example = builtin()[0].clone();
        example.name = "My Theme".into();
        example.accent = Hex(rgb(0xEB459E));
        let text = format!(
            "# Copy this file, change the name and colours, then press \"Reload themes\" in Appearance settings.\n{}",
            toml::to_string_pretty(&example).unwrap()
        );
        let _ = std::fs::write(dir.join("example.toml"), text);
    }

    let Ok(entries) = std::fs::read_dir(&dir) else { return (themes, errors) };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => crate::theme_editor::ThemeFormat::Toml,
            Some("css") => crate::theme_editor::ThemeFormat::Css,
            _ => continue,
        };
        {
            // CSS themes can leave properties out; those come from the default theme.
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| crate::theme_editor::parse(format, &t, &builtin()[0]));
            match parsed {
                Ok(t) => {
                    themes.retain(|existing| existing.name != t.name);
                    themes.push(t);
                }
                Err(e) => errors.push(format!("{}: {e}", path.file_name().unwrap_or_default().to_string_lossy())),
            }
        }
    }
    (themes, errors)
}

impl AppTheme {
    /// The iced theme used for widgets we don't style by hand.
    pub fn iced(&self) -> iced::Theme {
        iced::Theme::custom(
            self.name.clone(),
            theme::Palette {
                background: self.chat.0,
                text: self.text.0,
                primary: self.accent.0,
                success: self.online.0,
                warning: rgb(0xF0B232),
                danger: self.danger.0,
            },
        )
    }
}

pub fn luminance(c: Color) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

/// Black or white, whichever reads better on `bg`.
pub fn contrast_text(bg: Color) -> Color {
    if luminance(bg) > 0.6 { rgb(0x111111) } else { Color::WHITE }
}

pub fn with_alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let t = builtin().remove(0);
        let text = toml::to_string_pretty(&t).unwrap();
        assert!(text.contains("rail = \"#1E1F22\""));
        let back: AppTheme = toml::from_str(&text).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn hex_parse() {
        assert_eq!(Hex::parse("#ff0000"), Some(Color::from_rgb8(255, 0, 0)));
        assert_eq!(Hex::parse("00ff00"), Some(Color::from_rgb8(0, 255, 0)));
        assert_eq!(Hex::parse("#12"), None);
        assert_eq!(to_u32(rgb(0x5865F2)), 0x5865F2);
    }
}
