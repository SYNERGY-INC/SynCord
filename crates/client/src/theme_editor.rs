//! Theme editor in Appearance settings: edit a theme as TOML or as CSS-style
//! `--name: value;` lines, with a live preview. Theme files can be saved in either format.

use iced::widget::{Id, button, column, container, row, text, text_editor};
use iced::{Alignment, Element, Font, Length, Task};

use crate::app::{App, Message};
use crate::style::{self, BOLD, card};
use crate::theme::{AppTheme, Hex, builtin, themes_dir};

/// Which text format the editor shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeFormat {
    Toml,
    Css,
}

impl ThemeFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ThemeFormat::Toml => "toml",
            ThemeFormat::Css => "css",
        }
    }
}

pub struct ThemeEditor {
    pub content: text_editor::Content,
    pub format: ThemeFormat,
    /// Why the text doesn't work yet (the preview keeps the last version that did).
    pub error: Option<String>,
    /// Theme in use before editing, restored on Cancel.
    pub original: AppTheme,
}

const EDITOR_ID: &str = "theme-editor";

/// Every colour property, in the order they're written out.
const COLOURS: [&str; 13] = [
    "rail",
    "sidebar",
    "user_panel",
    "chat",
    "floating",
    "hover",
    "selected",
    "text",
    "muted",
    "header",
    "accent",
    "online",
    "danger",
];

fn colour_mut<'a>(t: &'a mut AppTheme, key: &str) -> Option<&'a mut Hex> {
    Some(match key {
        "rail" => &mut t.rail,
        "sidebar" => &mut t.sidebar,
        "user_panel" => &mut t.user_panel,
        "chat" => &mut t.chat,
        "floating" => &mut t.floating,
        "hover" => &mut t.hover,
        "selected" => &mut t.selected,
        "text" => &mut t.text,
        "muted" => &mut t.muted,
        "header" => &mut t.header,
        "accent" => &mut t.accent,
        "online" => &mut t.online,
        "danger" => &mut t.danger,
        _ => return None,
    })
}

fn hex_string(h: &Hex) -> String {
    let [r, g, b, a] = h.0.into_rgba8();
    if a == 255 { format!("#{r:02X}{g:02X}{b:02X}") } else { format!("#{r:02X}{g:02X}{b:02X}{a:02X}") }
}

pub fn to_toml(t: &AppTheme) -> String {
    toml::to_string_pretty(t).unwrap_or_default()
}

pub fn from_toml(s: &str) -> Result<AppTheme, String> {
    toml::from_str::<AppTheme>(s).map_err(|e| e.message().to_string())
}

pub fn to_css(t: &AppTheme) -> String {
    let mut t = t.clone();
    let mut out = String::from(
        "/* SynCord theme. Colours can be #RRGGBB, #RRGGBBAA, rgb(r, g, b) or rgba(r, g, b, a). */\n:root {\n",
    );
    out.push_str(&format!("  --name: \"{}\";\n", t.name.replace('"', "'")));
    for key in COLOURS {
        let h = colour_mut(&mut t, key).map(|h| hex_string(h)).unwrap_or_default();
        out.push_str(&format!("  --{}: {h};\n", key.replace('_', "-")));
    }
    out.push_str(&format!("  --font-size: {}px;\n", t.font_size));
    out.push_str(&format!("  --radius: {}px;\n", t.radius));
    out.push_str("}\n");
    out
}

/// Parse `rgb(30, 31, 34)` or `rgba(30, 31, 34, 0.5)`.
fn parse_rgb(v: &str) -> Option<iced::Color> {
    let inner = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb("))?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let channel = |s: &str| s.parse::<f32>().ok().filter(|n| (0.0..=255.0).contains(n)).map(|n| n as u8);
    match parts.as_slice() {
        [r, g, b] => Some(iced::Color::from_rgb8(channel(r)?, channel(g)?, channel(b)?)),
        [r, g, b, a] => {
            let a = a.parse::<f32>().ok().filter(|a| (0.0..=1.0).contains(a))?;
            Some(iced::Color::from_rgba8(channel(r)?, channel(g)?, channel(b)?, a))
        }
        _ => None,
    }
}

/// Parse CSS-style theme text. Properties left out keep their value from `base`.
pub fn from_css(s: &str, base: &AppTheme) -> Result<AppTheme, String> {
    // Drop /* comments */.
    let mut clean = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("/*") {
        clean.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return Err("A comment starts with /* but never ends with */".into()),
        }
    }
    clean.push_str(rest);

    let mut theme = base.clone();
    // Selectors and braces are optional; only `property: value;` pairs matter.
    let body: String = clean.chars().map(|c| if c == '{' || c == '}' { ';' } else { c }).collect();
    for decl in body.split(';') {
        let mut decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        // A selector in front of the first property (`:root --rail: ...`) is ignored.
        if let Some(i) = decl.find("--") {
            decl = &decl[i..];
        } else if decl.starts_with(':') || !decl.contains(':') {
            continue; // just a selector, like `:root`
        }
        let Some((key, value)) = decl.split_once(':') else { continue };
        let key = key.trim();
        let name = key.trim_start_matches("--").replace('-', "_");
        let value = value.trim();
        match name.as_str() {
            "name" => {
                let v = value.trim_matches(|c| c == '"' || c == '\'').trim();
                if v.is_empty() {
                    return Err("--name can't be empty".into());
                }
                theme.name = v.to_string();
            }
            "font_size" | "radius" => {
                let n: f32 = value.trim_end_matches("px").trim().parse().map_err(|_| {
                    format!("--{} should be a number like 15px, not \"{value}\"", key.trim_start_matches("--"))
                })?;
                if name == "font_size" {
                    if !(9.0..=30.0).contains(&n) {
                        return Err("--font-size should be between 9px and 30px".into());
                    }
                    theme.font_size = n;
                } else {
                    theme.radius = n.clamp(0.0, 30.0);
                }
            }
            other => {
                let known = || format!("--name, --{}, --font-size, --radius", COLOURS.join(", --").replace('_', "-"));
                let slot = colour_mut(&mut theme, other)
                    .ok_or_else(|| format!("Unknown property \"{key}\". Known ones: {}", known()))?;
                slot.0 = Hex::parse(value)
                    .or_else(|| parse_rgb(&value.replace(' ', "").replace(',', ", ")))
                    .ok_or_else(|| format!("--{} isn't a colour: \"{value}\"", key.trim_start_matches("--")))?;
            }
        }
    }
    Ok(theme)
}

/// Parse theme text in either format.
pub fn parse(format: ThemeFormat, s: &str, base: &AppTheme) -> Result<AppTheme, String> {
    match format {
        ThemeFormat::Toml => from_toml(s),
        ThemeFormat::Css => from_css(s, base),
    }
}

fn file_stem(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "theme".into() } else { out }
}

impl App {
    pub(crate) fn update_theme_editor(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenThemeEditor => {
                let mut start = self.theme.clone();
                // Don't overwrite a built-in theme; edit a copy.
                if builtin().iter().any(|b| b.name == start.name) {
                    start.name = format!("{} (custom)", start.name);
                }
                self.theme_editor = Some(ThemeEditor {
                    content: text_editor::Content::with_text(&to_css(&start)),
                    format: ThemeFormat::Css,
                    error: None,
                    original: self.theme.clone(),
                });
                return iced::widget::operation::focus(Id::new(EDITOR_ID));
            }
            Message::ThemeEditorAction(action) => {
                let Some(ed) = &mut self.theme_editor else { return Task::none() };
                let edited = action.is_edit();
                ed.content.perform(action);
                if edited {
                    match parse(ed.format, &ed.content.text(), &self.theme) {
                        Ok(t) => {
                            ed.error = None;
                            self.theme = t; // live preview
                        }
                        Err(e) => ed.error = Some(e),
                    }
                }
            }
            Message::ThemeEditorFormat(format) => {
                let Some(ed) = &mut self.theme_editor else { return Task::none() };
                if ed.format == format {
                    return Task::none();
                }
                match parse(ed.format, &ed.content.text(), &self.theme) {
                    Ok(t) => {
                        let text = match format {
                            ThemeFormat::Toml => to_toml(&t),
                            ThemeFormat::Css => to_css(&t),
                        };
                        ed.content = text_editor::Content::with_text(&text);
                        ed.format = format;
                        ed.error = None;
                    }
                    Err(e) => ed.error = Some(format!("Fix this before switching: {e}")),
                }
            }
            Message::SaveThemeEditor => {
                let Some(ed) = &mut self.theme_editor else { return Task::none() };
                let theme = match parse(ed.format, &ed.content.text(), &self.theme) {
                    Ok(t) => t,
                    Err(e) => {
                        ed.error = Some(e);
                        return Task::none();
                    }
                };
                if builtin().iter().any(|b| b.name == theme.name) {
                    ed.error = Some(format!("\"{}\" is a built-in theme. Give yours a different name.", theme.name));
                    return Task::none();
                }
                let Some(dir) = themes_dir() else {
                    ed.error = Some("Custom themes aren't supported on this system.".into());
                    return Task::none();
                };
                let stem = file_stem(&theme.name);
                let body = match ed.format {
                    ThemeFormat::Toml => format!("# SynCord theme, made in the theme editor.\n{}", ed.content.text()),
                    ThemeFormat::Css => ed.content.text(),
                };
                let path = dir.join(format!("{stem}.{}", ed.format.extension()));
                let result = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, body));
                if let Err(e) = result {
                    ed.error = Some(format!("Couldn't save {}: {e}", path.display()));
                    return Task::none();
                }
                // Only one file per theme: remove the copy in the other format, if any.
                for other in [ThemeFormat::Toml, ThemeFormat::Css] {
                    if other != ed.format {
                        let _ = std::fs::remove_file(dir.join(format!("{stem}.{}", other.extension())));
                    }
                }
                self.theme_editor = None;
                self.settings.theme = theme.name.clone();
                self.settings.save();
                return self.update(Message::ReloadThemes);
            }
            Message::CloseThemeEditor => {
                if let Some(ed) = self.theme_editor.take() {
                    self.theme = ed.original;
                }
            }
            _ => {}
        }
        Task::none()
    }

    pub(crate) fn theme_editor_view(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(ed) = &self.theme_editor else {
            return column![
                text("Edit the current theme as CSS or TOML and see the changes as you type.")
                    .size(fs * 0.9)
                    .color(t.text.0),
                button(text("Open theme editor").size(fs))
                    .padding([8, 16])
                    .on_press(Message::OpenThemeEditor)
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(10)
            .into();
        };

        let tab = |label: &'static str, format: ThemeFormat| {
            button(text(label).size(fs * 0.9).font(BOLD))
                .padding([6, 14])
                .on_press(Message::ThemeEditorFormat(format))
                .style(style::row_button(t, ed.format == format))
        };
        let (text_color, selection, muted, rail, accent, border) =
            (t.text.0, crate::theme::with_alpha(t.accent.0, 0.4), t.muted.0, t.rail.0, t.accent.0, t.hover.0);
        let radius = t.radius * 0.6;
        let editor = text_editor(&ed.content)
            .id(Id::new(EDITOR_ID))
            .on_action(Message::ThemeEditorAction)
            .font(Font::MONOSPACE)
            .size(fs * 0.9)
            .padding(12)
            .height(340)
            .style(move |_, status| text_editor::Style {
                background: rail.into(),
                border: iced::Border {
                    radius: radius.into(),
                    width: 1.0,
                    color: if matches!(status, text_editor::Status::Focused { .. }) { accent } else { border },
                },
                placeholder: muted,
                value: text_color,
                selection,
            });

        let status: Element<'_, Message> = match &ed.error {
            Some(e) => text(e.clone()).size(fs * 0.85).color(t.danger.0).into(),
            None => text("Looks good. Changes show live; Save keeps them.").size(fs * 0.85).color(t.online.0).into(),
        };
        let hint = match ed.format {
            ThemeFormat::Css => {
                "CSS-style: one --property: value; per line. Leave a line out to keep its current value."
            }
            ThemeFormat::Toml => "TOML: the same format as theme files. Every colour must be listed.",
        };

        container(
            column![
                row![
                    text("Theme editor").size(fs * 1.1).font(BOLD).color(t.header.0).width(Length::Fill),
                    tab("CSS", ThemeFormat::Css),
                    tab("TOML", ThemeFormat::Toml),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text(hint).size(fs * 0.8).color(t.muted.0),
                editor,
                status,
                row![
                    button(text("Save theme").size(fs))
                        .padding([8, 16])
                        .on_press_maybe(ed.error.is_none().then_some(Message::SaveThemeEditor))
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                    button(text("Cancel").size(fs))
                        .padding([8, 16])
                        .on_press(Message::CloseThemeEditor)
                        .style(style::link_button(t.text.0, t.header.0)),
                ]
                .spacing(8),
            ]
            .spacing(10),
        )
        .padding(14)
        .style(card(t.floating.0, t.radius))
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_roundtrip() {
        for t in builtin() {
            let css = to_css(&t);
            assert_eq!(from_css(&css, &builtin()[1]).unwrap(), t, "{css}");
            assert_eq!(from_toml(&to_toml(&t)).unwrap(), t);
        }
    }

    #[test]
    fn css_partial_and_flexible() {
        let base = builtin()[0].clone();
        let t = from_css("--accent: rgb(255, 0, 128);\n--name: 'Pink'; --font-size: 16px", &base).unwrap();
        assert_eq!(t.name, "Pink");
        assert_eq!(t.accent.0, iced::Color::from_rgb8(255, 0, 128));
        assert_eq!(t.font_size, 16.0);
        assert_eq!(t.chat, base.chat);
        // Selector on the same line, comments, no trailing semicolon.
        let t = from_css(":root { /* hi */ --user-panel: #112233 }", &base).unwrap();
        assert_eq!(t.user_panel.0, iced::Color::from_rgb8(0x11, 0x22, 0x33));
        let t = from_css("--hover: rgba(0, 0, 0, 0.5);", &base).unwrap();
        assert!((t.hover.0.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn css_errors_are_helpful() {
        let base = builtin()[0].clone();
        assert!(from_css("--acent: #fff;", &base).unwrap_err().contains("Unknown property"));
        assert!(from_css("--accent: blue;", &base).unwrap_err().contains("isn't a colour"));
        assert!(from_css("--font-size: big;", &base).unwrap_err().contains("number"));
        assert!(from_css("/* never closed", &base).unwrap_err().contains("comment"));
    }

    #[test]
    fn file_names() {
        assert_eq!(file_stem("Classic Dark (custom)"), "classic-dark-custom");
        assert_eq!(file_stem("!!!"), "theme");
    }
}
