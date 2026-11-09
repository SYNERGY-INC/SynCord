//! A server's own emoji, stickers and soundboard sounds (added in 0.7).
//!
//! Uploading goes over HTTP with `Authorization: Bearer <session token>`, like files:
//!
//! * `POST /api/emoji?name=NAME` with a PNG, GIF or WebP as the body (at most
//!   [`MAX_EMOJI_BYTES`]; bigger pictures are shrunk to [`EMOJI_SIZE`] if they aren't animated)
//!   answers with a [`CustomEmoji`]. `GET /api/emoji/<id>` downloads it.
//! * `POST /api/stickers?name=NAME` the same way for a [`Sticker`] ([`MAX_STICKER_BYTES`],
//!   [`STICKER_SIZE`]). `GET /api/stickers/<id>` downloads it.
//! * `POST /api/sounds?name=NAME&emoji=E` with an Ogg (Opus or Vorbis), WAV or MP3 file of
//!   at most [`MAX_SOUND_BYTES`] and [`MAX_SOUND_MS`], answers with a [`Sound`].
//!   `GET /api/sounds/<id>` downloads it as uploaded.
//!
//! Errors are a status code with a short explanation as plain text.
//! Uploading needs Manage emoji (emoji and stickers) or Manage soundboard (sounds).

use serde::{Deserialize, Serialize};

use crate::UserId;

pub type EmojiId = i64;
pub type StickerId = i64;
pub type SoundId = i64;

pub const EMOJI_PATH: &str = "/api/emoji";
pub const STICKERS_PATH: &str = "/api/stickers";
pub const SOUNDS_PATH: &str = "/api/sounds";

pub const MAX_EMOJI_BYTES: usize = 256 * 1024;
/// Largest emoji, in pixels (both ways).
pub const EMOJI_SIZE: u32 = 128;
pub const MAX_STICKER_BYTES: usize = 512 * 1024;
pub const STICKER_SIZE: u32 = 320;
pub const MAX_SOUND_BYTES: usize = 1024 * 1024;
/// Longest sound, in milliseconds.
pub const MAX_SOUND_MS: u32 = 5000;
pub const MAX_EMOJI: usize = 200;
pub const MAX_STICKERS: usize = 100;
pub const MAX_SOUNDS: usize = 100;

/// An emoji of this server. Written in messages (and reactions) as `<:name:id>`,
/// or `<a:name:id>` when animated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub id: EmojiId,
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    #[serde(default)]
    pub created_by: UserId,
}

impl CustomEmoji {
    /// How it's written in a message.
    pub fn code(&self) -> String {
        emoji_code(&self.name, self.id, self.animated)
    }
}

/// A big picture sent on its own as a message (see [`crate::ClientMsg::SendSticker`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    pub id: StickerId,
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    /// `image/png`, `image/gif` or `image/webp`.
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub created_by: UserId,
}

/// A short sound anyone in a voice channel can play for everyone there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sound {
    pub id: SoundId,
    pub name: String,
    /// Shown on its button.
    #[serde(default)]
    pub emoji: Option<String>,
    /// `audio/ogg`, `audio/wav` or `audio/mpeg`.
    pub mime: String,
    /// How long it plays (0 if the server couldn't tell).
    #[serde(default)]
    pub duration_ms: u32,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub created_by: UserId,
}

/// Names (shortcodes) are 2–32 letters, digits and underscores.
pub fn valid_expression_name(name: &str) -> bool {
    (2..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `<:name:id>`, or `<a:name:id>` for an animated emoji.
pub fn emoji_code(name: &str, id: EmojiId, animated: bool) -> String {
    format!("<{}:{name}:{id}>", if animated { "a" } else { "" })
}

/// A custom emoji written in text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiRef<'a> {
    pub name: &'a str,
    pub id: EmojiId,
    pub animated: bool,
    /// Byte range of the whole `<:name:id>` in the text.
    pub range: std::ops::Range<usize>,
}

/// Every custom emoji in `text`, in order.
pub fn find_custom_emoji(text: &str) -> Vec<EmojiRef<'_>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find('<') {
        let start = from + i;
        from = start + 1;
        let rest = &text[start + 1..];
        let (animated, body) = match rest.strip_prefix("a:") {
            Some(b) => (true, b),
            None => match rest.strip_prefix(':') {
                Some(b) => (false, b),
                None => continue,
            },
        };
        let Some(end) = body.find('>') else { continue };
        let Some((name, id)) = body[..end].split_once(':') else { continue };
        let Ok(id) = id.parse::<EmojiId>() else { continue };
        if !valid_expression_name(name) {
            continue;
        }
        let len = 1 + (rest.len() - body.len()) + end + 1;
        out.push(EmojiRef { name, id, animated, range: start..start + len });
        from = start + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_custom_emoji() {
        let text = "hi <:wave:12> and <a:party_parrot:7>! not <:x:1> <:bad name:3> <:ok:nope> <:end";
        let found = find_custom_emoji(text);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].name, found[0].id, found[0].animated), ("wave", 12, false));
        assert_eq!(&text[found[0].range.clone()], "<:wave:12>");
        assert_eq!((found[1].name, found[1].id, found[1].animated), ("party_parrot", 7, true));
        assert_eq!(&text[found[1].range.clone()], "<a:party_parrot:7>");
        assert_eq!(emoji_code("wave", 12, false), "<:wave:12>");
        assert!(valid_expression_name("cat_2"));
        assert!(!valid_expression_name("c"));
        assert!(!valid_expression_name("smile!"));
    }
}
