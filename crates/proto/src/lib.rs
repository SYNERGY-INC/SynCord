//! Wire protocol shared by the SynCord server and client.
//!
//! Every frame on the WebSocket is a single JSON object: the client sends
//! [`ClientMsg`] and the server replies / pushes [`ServerMsg`].

use serde::{Deserialize, Serialize};

pub mod voice;

pub type UserId = i64;
pub type ChannelId = i64;
pub type MessageId = i64;

/// Maximum length of a chat message, in characters.
pub const MAX_MESSAGE_LEN: usize = 4000;
/// Number of messages returned per history page.
pub const HISTORY_PAGE: usize = 50;
/// Most files that can be attached to one message.
pub const MAX_ATTACHMENTS: usize = 10;
/// Most different emoji reactions on one message.
pub const MAX_REACTIONS: usize = 20;

/// HTTP endpoint for uploading a file: `POST /api/upload?channel_id=N&filename=NAME`
/// with the raw file as the body and `Authorization: Bearer <session token>`.
/// Answers with an [`Attachment`] as JSON.
pub const UPLOAD_PATH: &str = "/api/upload";
/// HTTP endpoint for downloading: `GET /api/files/<attachment id>` (add `?thumb=1`
/// for the small preview of an image), with the same `Authorization` header.
pub const FILES_PATH: &str = "/api/files";

/// A user's public profile, including all the cosmetic customization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub profile: Profile,
    pub is_owner: bool,
}

impl User {
    pub fn name(&self) -> &str {
        if self.profile.display_name.trim().is_empty() { &self.username } else { &self.profile.display_name }
    }
}

/// Everything a user can customize about how they appear to others.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub display_name: String,
    /// Shown under the name in the member list and on the profile card.
    pub status_text: String,
    pub bio: String,
    /// Colour of the name and avatar background (0xRRGGBB).
    pub accent_color: u32,
    /// Colour of the profile card banner (0xRRGGBB).
    pub banner_color: u32,
    /// A short glyph (one or two characters) drawn on the avatar instead of the initial.
    pub avatar_glyph: String,
    /// Decorative ring drawn around the avatar.
    pub avatar_ring: AvatarRing,
    /// Colour of the avatar ring (0xRRGGBB).
    pub ring_color: u32,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            status_text: String::new(),
            bio: String::new(),
            accent_color: 0x5865F2,
            banner_color: 0x5865F2,
            avatar_glyph: String::new(),
            avatar_ring: AvatarRing::None,
            ring_color: 0xFFFFFF,
        }
    }
}

impl Profile {
    /// Clamp user-supplied values to sane sizes.
    pub fn sanitized(mut self) -> Self {
        fn clip(s: &mut String, max: usize) {
            *s = s.trim().chars().filter(|c| !c.is_control()).take(max).collect();
        }
        clip(&mut self.display_name, 32);
        clip(&mut self.status_text, 64);
        clip(&mut self.avatar_glyph, 2);
        self.bio = self.bio.chars().take(400).collect();
        self.accent_color &= 0xFFFFFF;
        self.banner_color &= 0xFFFFFF;
        self.ring_color &= 0xFFFFFF;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AvatarRing {
    #[default]
    None,
    Thin,
    Thick,
    Double,
}

impl AvatarRing {
    pub const ALL: [AvatarRing; 4] = [Self::None, Self::Thin, Self::Thick, Self::Double];
}

impl std::fmt::Display for AvatarRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Thin => "Thin ring",
            Self::Thick => "Thick ring",
            Self::Double => "Double ring",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ChannelKind {
    #[default]
    Text,
    Voice,
    /// A private conversation between two people.
    Direct,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub topic: String,
    pub position: i64,
    #[serde(default)]
    pub kind: ChannelKind,
    /// Newest message in the channel, used to show unread markers.
    #[serde(default)]
    pub last_message_id: Option<MessageId>,
}

/// A direct-message conversation, as seen by one of its two members.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectChannel {
    pub id: ChannelId,
    /// The other person.
    pub user_id: UserId,
    #[serde(default)]
    pub last_message_id: Option<MessageId>,
}

/// The newest message someone has seen in a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    pub channel_id: ChannelId,
    pub last_read: MessageId,
}

/// A file attached to a message. Images have a size and a small preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Random hex id, also used in the download URL.
    pub id: String,
    pub filename: String,
    pub size: u64,
    pub mime: String,
    /// Set for images (pixel size of the original).
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// True for animated GIFs.
    #[serde(default)]
    pub animated: bool,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.width.is_some() && self.height.is_some()
    }
}

/// A GIF from the server's GIF search (GIPHY or KLIPY).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifResult {
    pub id: String,
    pub title: String,
    /// Small animated preview to show in the picker.
    pub preview_url: String,
    /// A still frame of the preview (cheaper to show many of).
    #[serde(default)]
    pub still_url: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// One emoji reaction on a message and who added it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub users: Vec<UserId>,
}

/// Who is in which voice channel, and whether they're muted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceState {
    pub user_id: UserId,
    pub channel_id: ChannelId,
    pub muted: bool,
    pub deafened: bool,
    /// Identifies this person's audio packets (see [`voice`]).
    pub ssrc: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub author_id: UserId,
    pub content: String,
    /// Unix timestamp in milliseconds.
    pub created_at: i64,
    /// Unix timestamp in milliseconds of the last edit, if any.
    pub edited_at: Option<i64>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
}

/// Snapshot sent right after a successful login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ready {
    pub token: String,
    pub me: UserId,
    pub community_name: String,
    pub users: Vec<User>,
    pub online: Vec<UserId>,
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub voice: Vec<VoiceState>,
    /// Your direct-message conversations.
    #[serde(default)]
    pub directs: Vec<DirectChannel>,
    /// What you've already read, per channel.
    #[serde(default)]
    pub read_states: Vec<ReadState>,
    /// The server program's version (empty for servers older than 0.3).
    #[serde(default)]
    pub server_version: String,
    /// Name of the GIF search service ("GIPHY" or "KLIPY"), if the server has one set up.
    #[serde(default)]
    pub gif_provider: Option<String>,
    /// Largest upload the server accepts, in MB (0 for servers older than 0.4).
    #[serde(default)]
    pub max_upload_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "d")]
pub enum ClientMsg {
    Register {
        username: String,
        password: String,
        invite: String,
    },
    Login {
        username: String,
        password: String,
    },
    Resume {
        token: String,
    },
    Logout,
    SendMessage {
        channel_id: ChannelId,
        content: String,
        /// Ids of files uploaded beforehand (see [`UPLOAD_PATH`]).
        #[serde(default)]
        attachments: Vec<String>,
    },
    EditMessage {
        message_id: MessageId,
        content: String,
    },
    DeleteMessage {
        message_id: MessageId,
    },
    FetchHistory {
        channel_id: ChannelId,
        before: Option<MessageId>,
    },
    Typing {
        channel_id: ChannelId,
    },
    UpdateProfile {
        profile: Profile,
    },
    CreateChannel {
        name: String,
        topic: String,
        #[serde(default)]
        kind: ChannelKind,
    },
    DeleteChannel {
        channel_id: ChannelId,
    },
    AddReaction {
        message_id: MessageId,
        emoji: String,
    },
    RemoveReaction {
        message_id: MessageId,
        emoji: String,
    },
    /// Open (or create) a direct-message conversation with someone.
    OpenDirect {
        user_id: UserId,
    },
    /// Search for GIFs (an empty query means trending GIFs).
    SearchGifs {
        query: String,
    },
    /// Post a GIF from the search results; the server downloads and attaches it.
    SendGif {
        channel_id: ChannelId,
        gif_id: String,
    },
    /// Remember that you've read a channel up to this message.
    MarkRead {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    JoinVoice {
        channel_id: ChannelId,
    },
    LeaveVoice,
    SetVoiceState {
        muted: bool,
        deafened: bool,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "d")]
pub enum ServerMsg {
    Ready(Ready),
    AuthFailed {
        reason: String,
    },
    MessageCreated(Message),
    MessageUpdated(Message),
    MessageDeleted {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    History {
        channel_id: ChannelId,
        messages: Vec<Message>,
        reached_start: bool,
    },
    Typing {
        channel_id: ChannelId,
        user_id: UserId,
    },
    UserJoined(User),
    UserUpdated(User),
    Presence {
        user_id: UserId,
        online: bool,
    },
    ChannelCreated(Channel),
    ChannelDeleted {
        channel_id: ChannelId,
    },
    GifResults {
        query: String,
        results: Vec<GifResult>,
    },
    /// A direct-message conversation was opened (sent only to its two members).
    DirectOpened(DirectChannel),
    ReactionUpdate {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: String,
        user_id: UserId,
        added: bool,
    },
    /// Sent only to the joining connection: how to start sending audio.
    VoiceReady {
        channel_id: ChannelId,
        ssrc: u32,
        /// Hex-encoded secret that must prefix every audio packet this client sends.
        token: String,
        /// UDP port for audio. If UDP can't get through, audio also works over this WebSocket.
        udp_port: u16,
    },
    /// Someone joined, left (`state: None`) or changed mute/deafen.
    VoiceStateUpdate {
        user_id: UserId,
        state: Option<VoiceState>,
    },
    Error {
        message: String,
    },
    Pong,
}

/// Normalize a channel name the same way on both ends: lowercase, dashes for spaces.
pub fn normalize_channel_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if (c == '-' || c.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(32).collect()
}

/// Whether a reaction string is acceptable: short, one "word", no control characters.
pub fn valid_reaction(emoji: &str) -> bool {
    !emoji.is_empty() && emoji.len() <= 32 && !emoji.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Tidy a file name for storage and display: no folders, no control characters.
pub fn clean_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(100).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

/// Tidy a voice channel name: voice channels keep their capitals and spaces.
pub fn clean_voice_channel_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names() {
        assert_eq!(normalize_channel_name("  General Chat!! "), "general-chat");
        assert_eq!(normalize_channel_name("memes -- and  stuff"), "memes-and-stuff");
        assert_eq!(normalize_channel_name("!!!"), "");
    }

    #[test]
    fn roundtrip() {
        let msg = ClientMsg::SendMessage { channel_id: 3, content: "hi".into(), attachments: vec![] };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"op":"SendMessage","d":{"channel_id":3,"content":"hi","attachments":[]}}"#);
        // Older clients that don't send attachments still work.
        let old: ClientMsg =
            serde_json::from_str(r#"{"op":"SendMessage","d":{"channel_id":3,"content":"hi"}}"#).unwrap();
        assert!(matches!(old, ClientMsg::SendMessage { attachments, .. } if attachments.is_empty()));
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ClientMsg::SendMessage { channel_id: 3, .. }));
    }

    #[test]
    fn filenames_and_reactions() {
        assert_eq!(clean_filename("C:\\Users\\me\\cat.png"), "cat.png");
        assert_eq!(clean_filename("../../etc/passwd"), "passwd");
        assert_eq!(clean_filename(".hidden"), "hidden");
        assert_eq!(clean_filename(""), "file");
        assert!(valid_reaction("👍"));
        assert!(valid_reaction("👨‍👩‍👧"));
        assert!(!valid_reaction("a b"));
        assert!(!valid_reaction(""));
    }

    #[test]
    fn profile_sanitize() {
        let p = Profile { display_name: "x".repeat(100), accent_color: 0xFF123456, ..Default::default() }.sanitized();
        assert_eq!(p.display_name.len(), 32);
        assert_eq!(p.accent_color, 0x123456);
    }
}
