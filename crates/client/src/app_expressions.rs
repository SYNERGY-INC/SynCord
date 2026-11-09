//! The server's own emoji and stickers (0.7): loading their pictures, typing and sending
//! them, and uploading, renaming and deleting them in the server settings. Soundboard
//! sounds are managed here too (playing them is in soundboard.rs).

use std::path::PathBuf;

use celerity_proto::expressions::{
    EMOJI_PATH, SOUNDS_PATH, STICKERS_PATH, emoji_code, find_custom_emoji, valid_expression_name,
};
use celerity_proto::{ClientMsg, CustomEmoji, ServerMsg, StickerId};
use iced::Task;

use crate::app::{App, Message};
use crate::features::ConfirmAction;
use crate::media;

/// Emoji are drawn at most this big (2x for sharp screens).
const EMOJI_DECODE: u32 = 96;
const STICKER_DECODE: u32 = 320;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprKind {
    Emoji,
    Sticker,
    Sound,
}

impl ExprKind {
    fn path(self) -> &'static str {
        match self {
            ExprKind::Emoji => EMOJI_PATH,
            ExprKind::Sticker => STICKERS_PATH,
            ExprKind::Sound => SOUNDS_PATH,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            ExprKind::Emoji => "emoji",
            ExprKind::Sticker => "sticker",
            ExprKind::Sound => "sound",
        }
    }
}

/// Renaming something in the Emoji or Soundboard settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ExprEdit {
    pub kind: ExprKind,
    pub id: i64,
    pub name: String,
    /// A sound's button emoji.
    pub emoji: String,
}

#[derive(Default)]
pub struct ExprUi {
    pub editing: Option<ExprEdit>,
    /// What's being uploaded.
    pub uploading: Option<ExprKind>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ExprMsg {
    Upload(ExprKind),
    Chosen(ExprKind, Option<PathBuf>),
    Uploaded(Result<(), String>),
    StartEdit(ExprKind, i64),
    EditName(String),
    EditEmoji(String),
    SaveEdit,
    CancelEdit,
    AskDelete(ExprKind, i64),
    SendSticker(StickerId),
}

/// Cache key for an emoji's or sticker's picture (ids are per server, so the server is part of it).
pub fn picture_key(base: &str, kind: ExprKind, id: i64) -> String {
    format!("{}:{base}:{id}", kind.noun())
}

/// A file name as an emoji name: letters, digits and underscores, 2 to 32 of them.
pub fn name_from_file(stem: &str) -> String {
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out: String = out.trim_end_matches('_').chars().take(32).collect();
    if valid_expression_name(&out) { out } else { "emoji".into() }
}

/// Turn `:name:` into the code of the server's emoji with that name (unless it's also a
/// standard emoji's shortcode) and the standard `:shortcodes:` into emoji, leaving emoji
/// codes already in the text (`<:name:id>`) alone.
pub fn expand_emoji(text: &str, custom: &[CustomEmoji]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for r in find_custom_emoji(text) {
        out.push_str(&expand_plain(&text[last..r.range.start], custom));
        out.push_str(&text[r.range.clone()]);
        last = r.range.end;
    }
    out.push_str(&expand_plain(&text[last..], custom));
    out
}

/// Text without emoji codes in it.
fn expand_plain(text: &str, custom: &[CustomEmoji]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut plain_from = 0;
    let mut i = 0;
    while let Some(off) = text[i..].find(':') {
        let start = i + off;
        let after = &text[start + 1..];
        let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
        let name = &after[..end];
        let closed = after[end..].starts_with(':');
        let found = closed
            .then(|| custom.iter().find(|e| e.name.eq_ignore_ascii_case(name)))
            .flatten()
            .filter(|_| emojis::get_by_shortcode(name).is_none());
        match found {
            Some(e) => {
                out.push_str(&crate::emoji::replace_shortcodes(&text[plain_from..start]));
                out.push_str(&emoji_code(&e.name, e.id, e.animated));
                i = start + 1 + end + 1;
                plain_from = i;
            }
            None => i = start + 1,
        }
    }
    out.push_str(&crate::emoji::replace_shortcodes(&text[plain_from..]));
    out
}

/// Server emoji whose names match a half-typed `:code`, best matches first.
pub fn custom_suggestions<'a>(custom: &'a [CustomEmoji], code: &str, limit: usize) -> Vec<&'a CustomEmoji> {
    let code = code.to_lowercase();
    let mut starts: Vec<&CustomEmoji> = custom.iter().filter(|e| e.name.to_lowercase().starts_with(&code)).collect();
    starts.sort_by_key(|e| e.name.len());
    let contains = custom.iter().filter(|e| {
        let n = e.name.to_lowercase();
        n.contains(&code) && !n.starts_with(&code)
    });
    starts.into_iter().chain(contains).take(limit).collect()
}

/// Upload a picture or sound; the server answers and tells everyone about it.
async fn upload(
    http: reqwest::Client,
    base: String,
    token: String,
    kind: ExprKind,
    path: PathBuf,
    query: Vec<(&'static str, String)>,
) -> Result<(), String> {
    let bytes = tokio::fs::read(&path).await.map_err(|e| format!("Couldn't read the file: {e}"))?;
    let url = reqwest::Url::parse_with_params(&format!("{base}{}", kind.path()), &query)
        .map_err(|e| format!("Bad server address: {e}"))?;
    let resp = http.post(url).bearer_auth(token).body(bytes).send().await.map_err(|e| format!("Upload failed: {e}"))?;
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status().as_u16();
    let why = resp.text().await.unwrap_or_default();
    let why = why.trim();
    Err(match (status, why.is_empty()) {
        (404 | 405, _) => "This server doesn't support that yet: it needs the latest Celerity Server.".into(),
        (_, false) => {
            let mut chars = why.chars();
            let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
            format!("{first}{}.", chars.as_str().trim_end_matches('.'))
        }
        (code, true) => format!("The server answered with error {code}."),
    })
}

/// Download and decode an emoji or sticker (animated ones keep their frames).
async fn load_picture(
    http: reqwest::Client,
    url: String,
    token: String,
    side: u32,
) -> Result<std::sync::Arc<media::Picture>, String> {
    let resp = http.get(url).bearer_auth(token).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("error {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || media::decode(&bytes, side))
        .await
        .map_err(|e| e.to_string())?
        .map(std::sync::Arc::new)
}

impl App {
    pub(crate) fn expression_key(&self, kind: ExprKind, id: i64) -> String {
        picture_key(&self.server_base(), kind, id)
    }

    /// `:name:` and standard shortcodes in a message about to be sent.
    pub(crate) fn expand_custom_emoji(&self, text: &str) -> String {
        let custom = self.session.as_ref().map(|s| s.emojis.as_slice()).unwrap_or(&[]);
        expand_emoji(text, custom)
    }

    /// Load the server's emoji, and the stickers shown right now (in messages, the picker
    /// or the settings), unless they're loaded, on their way or failed already.
    pub(crate) fn request_expressions(&mut self) -> Task<Message> {
        let Some(s) = &self.session else { return Task::none() };
        let mut wanted: Vec<(ExprKind, i64)> = s.emojis.iter().map(|e| (ExprKind::Emoji, e.id)).collect();
        let all_stickers = matches!(self.emoji_picker.as_ref().map(|p| p.tab), Some(crate::app::PickerTab::Stickers))
            || self.settings_tab == Some(crate::app::SettingsTab::Emoji);
        if all_stickers {
            wanted.extend(s.stickers.iter().map(|x| (ExprKind::Sticker, x.id)));
        }
        let shown = [s.current, s.open_thread].into_iter().flatten().filter_map(|c| s.chans.get(&c));
        for st in shown {
            wanted.extend(st.messages.iter().filter_map(|m| m.sticker.as_ref()).map(|x| (ExprKind::Sticker, x.id)));
        }
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        let mut tasks = Vec::new();
        for (kind, id) in wanted {
            let key = picture_key(&base, kind, id);
            if matches!(self.images.get(&key), Some(media::Entry::Failed)) || !self.images.start(&key) {
                continue;
            }
            let side = if kind == ExprKind::Sticker { STICKER_DECODE } else { EMOJI_DECODE };
            let url = format!("{base}{}/{id}", kind.path());
            tasks.push(Task::perform(load_picture(http.clone(), url, token.clone(), side), move |r| {
                Message::PictureLoaded(key.clone(), r)
            }));
        }
        Task::batch(tasks)
    }

    pub(crate) fn update_expressions(&mut self, msg: ExprMsg) -> Task<Message> {
        match msg {
            ExprMsg::Upload(kind) => {
                let (title, filter): (&str, &[&str]) = match kind {
                    ExprKind::Emoji => ("Choose an emoji picture", &["png", "gif", "webp"]),
                    ExprKind::Sticker => ("Choose a sticker picture", &["png", "gif", "webp"]),
                    ExprKind::Sound => ("Choose a sound", &["ogg", "opus", "oga", "wav", "mp3"]),
                };
                let name = if kind == ExprKind::Sound { "Sounds" } else { "Pictures" };
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .add_filter(name, filter)
                            .pick_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    move |path| Message::Expressions(ExprMsg::Chosen(kind, path)),
                );
            }
            ExprMsg::Chosen(_, None) => {}
            ExprMsg::Chosen(kind, Some(path)) => {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let query = match kind {
                    ExprKind::Sound => {
                        let name: String = stem.replace(['_', '-'], " ").trim().chars().take(32).collect();
                        vec![("name", if name.is_empty() { "New sound".into() } else { name })]
                    }
                    _ => vec![("name", self.unused_name(kind, &name_from_file(&stem)))],
                };
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                self.expr.uploading = Some(kind);
                self.expr.error = None;
                return Task::perform(upload(http, base, token, kind, path, query), move |r| {
                    Message::Expressions(ExprMsg::Uploaded(r))
                });
            }
            ExprMsg::Uploaded(result) => {
                self.expr.uploading = None;
                self.expr.error = result.err();
            }
            ExprMsg::StartEdit(kind, id) => {
                let Some(s) = &self.session else { return Task::none() };
                let found = match kind {
                    ExprKind::Emoji => s.emojis.iter().find(|e| e.id == id).map(|e| (e.name.clone(), String::new())),
                    ExprKind::Sticker => {
                        s.stickers.iter().find(|e| e.id == id).map(|e| (e.name.clone(), String::new()))
                    }
                    ExprKind::Sound => s
                        .sounds
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| (e.name.clone(), e.emoji.clone().unwrap_or_default())),
                };
                if let Some((name, emoji)) = found {
                    self.expr.editing = Some(ExprEdit { kind, id, name, emoji });
                    return iced::widget::operation::focus(iced::widget::Id::new("expr-name"));
                }
            }
            ExprMsg::EditName(v) => {
                if let Some(e) = &mut self.expr.editing {
                    e.name = v;
                }
            }
            ExprMsg::EditEmoji(v) => {
                if let Some(e) = &mut self.expr.editing {
                    e.emoji = v.chars().take(8).collect();
                }
            }
            ExprMsg::SaveEdit => {
                let Some(e) = self.expr.editing.take() else { return Task::none() };
                let name = e.name.trim().to_string();
                let msg = match e.kind {
                    ExprKind::Emoji => ClientMsg::RenameEmoji { emoji_id: e.id, name },
                    ExprKind::Sticker => ClientMsg::RenameSticker { sticker_id: e.id, name },
                    ExprKind::Sound => ClientMsg::UpdateSound {
                        sound_id: e.id,
                        name,
                        emoji: Some(e.emoji.trim().to_string()).filter(|x| !x.is_empty()),
                    },
                };
                self.send(msg);
            }
            ExprMsg::CancelEdit => self.expr.editing = None,
            ExprMsg::AskDelete(kind, id) => {
                let Some(s) = &self.session else { return Task::none() };
                let name = match kind {
                    ExprKind::Emoji => s.emojis.iter().find(|e| e.id == id).map(|e| format!(":{}:", e.name)),
                    ExprKind::Sticker => s.stickers.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                    ExprKind::Sound => s.sounds.iter().find(|e| e.id == id).map(|e| e.name.clone()),
                };
                let Some(name) = name else { return Task::none() };
                let action = match kind {
                    ExprKind::Emoji => ConfirmAction::DeleteEmoji(id),
                    ExprKind::Sticker => ConfirmAction::DeleteSticker(id),
                    ExprKind::Sound => ConfirmAction::DeleteSound(id),
                };
                let noun = kind.noun();
                self.ask(
                    format!("Delete {noun}"),
                    format!("Delete {name}? It's gone for everyone on this server."),
                    format!("Delete {}{}", noun[..1].to_uppercase(), &noun[1..]),
                    action,
                );
            }
            ExprMsg::SendSticker(sticker_id) => {
                let Some(channel_id) = self.session.as_ref().and_then(|s| s.current) else { return Task::none() };
                self.emoji_picker = None;
                let reply_to = self.replying.take();
                self.send(ClientMsg::SendSticker { channel_id, sticker_id, reply_to });
                return iced::widget::operation::snap_to(
                    iced::widget::Id::new(crate::app::MESSAGES_ID),
                    iced::widget::operation::RelativeOffset::START,
                );
            }
        }
        Task::none()
    }

    /// `name`, or `name_2`, `name_3`... if the server already has one called that.
    fn unused_name(&self, kind: ExprKind, name: &str) -> String {
        let Some(s) = &self.session else { return name.to_string() };
        let taken = |n: &str| match kind {
            ExprKind::Emoji => s.emojis.iter().any(|e| e.name.eq_ignore_ascii_case(n)),
            _ => s.stickers.iter().any(|e| e.name.eq_ignore_ascii_case(n)),
        };
        if !taken(name) {
            return name.to_string();
        }
        (2..100)
            .map(|i| format!("{}_{i}", &name[..name.len().min(28)]))
            .find(|n| !taken(n))
            .unwrap_or_else(|| name.to_string())
    }

    pub(crate) fn on_expression_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let base = self.server_base();
        let Some(s) = &mut self.session else { return Task::none() };
        let key = |kind, id| picture_key(&base, kind, id);
        match msg {
            ServerMsg::EmojiUpdated(e) => {
                // A new picture for the same id is never sent, so a rename keeps the loaded one.
                match s.emojis.iter_mut().find(|x| x.id == e.id) {
                    Some(slot) => *slot = e,
                    None => s.emojis.push(e),
                }
                return self.request_expressions();
            }
            ServerMsg::EmojiDeleted { emoji_id } => {
                s.emojis.retain(|e| e.id != emoji_id);
                let k = key(ExprKind::Emoji, emoji_id);
                self.images.remove(&k);
            }
            ServerMsg::StickerUpdated(x) => match s.stickers.iter_mut().find(|y| y.id == x.id) {
                Some(slot) => *slot = x,
                None => {
                    s.stickers.push(x);
                    return self.request_expressions();
                }
            },
            ServerMsg::StickerDeleted { sticker_id } => {
                s.stickers.retain(|x| x.id != sticker_id);
                let k = key(ExprKind::Sticker, sticker_id);
                self.images.remove(&k);
            }
            ServerMsg::SoundUpdated(x) => match s.sounds.iter_mut().find(|y| y.id == x.id) {
                Some(slot) => *slot = x,
                None => s.sounds.push(x),
            },
            ServerMsg::SoundDeleted { sound_id } => s.sounds.retain(|x| x.id != sound_id),
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn party() -> Vec<CustomEmoji> {
        vec![
            CustomEmoji { id: 5, name: "party".into(), animated: true, created_by: 1 },
            CustomEmoji { id: 6, name: "wave".into(), animated: false, created_by: 1 },
        ]
    }

    #[test]
    fn expands_server_emoji_and_shortcodes() {
        let custom = party();
        assert_eq!(expand_emoji("yay :party: :smile:", &custom), "yay <a:party:5> 😄");
        // A standard shortcode wins over a server emoji with the same name.
        assert_eq!(expand_emoji(":wave:", &custom), "👋");
        // Codes already written stay as they are, even when their names are shortcodes.
        assert_eq!(expand_emoji("<:wave:6> and <a:party:5>", &custom), "<:wave:6> and <a:party:5>");
        assert_eq!(expand_emoji("time 10:30:45 :nope:", &custom), "time 10:30:45 :nope:");
        assert_eq!(expand_emoji(":PARTY::party:", &custom), "<a:party:5><a:party:5>");
    }

    #[test]
    fn suggests_server_emoji() {
        let custom = party();
        let names: Vec<&str> = custom_suggestions(&custom, "par", 5).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["party"]);
        let names: Vec<&str> = custom_suggestions(&custom, "a", 5).iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["party", "wave"]);
    }

    #[test]
    fn names_from_files() {
        assert_eq!(name_from_file("Party Parrot (1)"), "Party_Parrot_1");
        assert_eq!(name_from_file("ü"), "emoji");
        assert_eq!(name_from_file(&"x".repeat(40)).len(), 32);
        assert_eq!(picture_key("http://h:1", ExprKind::Emoji, 3), "emoji:http://h:1:3");
    }
}
