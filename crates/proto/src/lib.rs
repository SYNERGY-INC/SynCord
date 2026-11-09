//! Wire protocol shared by the Celerity server and client.
//!
//! Every frame on the WebSocket is a single JSON object: the client sends
//! [`ClientMsg`] and the server replies / pushes [`ServerMsg`].

use serde::{Deserialize, Serialize};

pub mod e2ee;
pub mod expressions;
pub mod invites;
pub mod social;
pub mod voice;

pub use e2ee::{EncryptedPayload, IdentityKeys, WrappedKey};
pub use expressions::{CustomEmoji, EmojiId, Sound, SoundId, Sticker, StickerId};
pub use invites::Invite;
pub use social::{
    Category, CategoryId, CategoryPosition, ChannelPosition, GroupChannel, Poll, PollOption, Thread, ThreadSummary,
};

pub type UserId = i64;
pub type ChannelId = i64;
pub type MessageId = i64;
pub type RoleId = i64;

/// The role everyone has. It can't be deleted; its permissions are the defaults.
pub const EVERYONE_ROLE: RoleId = 1;
/// Most roles a server can have.
pub const MAX_ROLES: usize = 50;

/// What apps send in the WebSocket URL to say which protocol they speak: `/ws?protocol=7`.
/// Apps older than 0.7 don't send it; the server then leaves out events they can't show
/// (thread and group messages) and keeps role permissions they don't know about.
pub const PROTOCOL_VERSION: u32 = 7;

/// What roles allow, as bits in [`Role::permissions`].
pub mod perms {
    /// Everything below, and roles are no limit (except on the owner).
    pub const ADMINISTRATOR: u64 = 1 << 0;
    /// Create and delete channels.
    pub const MANAGE_CHANNELS: u64 = 1 << 1;
    /// Create, edit and hand out roles lower than your own.
    pub const MANAGE_ROLES: u64 = 1 << 2;
    /// Delete other people's messages.
    pub const MANAGE_MESSAGES: u64 = 1 << 3;
    /// Ping everyone with @everyone.
    pub const MENTION_EVERYONE: u64 = 1 << 4;
    pub const SEND_MESSAGES: u64 = 1 << 5;
    pub const ATTACH_FILES: u64 = 1 << 6;
    pub const CONNECT_VOICE: u64 = 1 << 7;
    /// Links in your messages get a preview.
    pub const EMBED_LINKS: u64 = 1 << 8;
    pub const ADD_REACTIONS: u64 = 1 << 9;
    /// Drag people from one voice channel to another.
    pub const MOVE_MEMBERS: u64 = 1 << 10;
    /// Server-mute and server-deafen people in voice.
    pub const MUTE_MEMBERS: u64 = 1 << 11;
    /// Add, rename and delete the server's custom emoji and stickers (0.7).
    pub const MANAGE_EMOJI: u64 = 1 << 12;
    /// Add, rename and delete soundboard sounds (0.7).
    pub const MANAGE_SOUNDBOARD: u64 = 1 << 13;
    /// Make, list and revoke invite links (0.7).
    pub const CREATE_INVITES: u64 = 1 << 14;
    /// Remove people from the server; they can come back with a new invite (0.7).
    pub const KICK_MEMBERS: u64 = 1 << 15;
    /// Remove people for good (until unbanned) (0.7).
    pub const BAN_MEMBERS: u64 = 1 << 16;
    /// Play soundboard sounds in voice channels (0.7).
    pub const USE_SOUNDBOARD: u64 = 1 << 17;

    pub const ALL: u64 = (1 << 18) - 1;
    /// Permissions apps older than 0.7 don't know about (they leave them untouched).
    pub const SINCE_0_7: u64 = ALL & !((1 << 12) - 1);
    /// What the everyone role starts with. Moving and muting people is for roles the
    /// owner hands out (or turns on for everyone), like Discord.
    pub const DEFAULT: u64 =
        SEND_MESSAGES | ATTACH_FILES | CONNECT_VOICE | EMBED_LINKS | ADD_REACTIONS | USE_SOUNDBOARD;

    /// Every permission with a name and an explanation, in the order the settings show them.
    pub const LIST: &[(u64, &str, &str)] = &[
        (ADMINISTRATOR, "Administrator", "Every permission. Give this one carefully."),
        (MANAGE_CHANNELS, "Manage channels", "Create and delete channels."),
        (MANAGE_ROLES, "Manage roles", "Create and edit roles below their own, and give them to people."),
        (MANAGE_MESSAGES, "Manage messages", "Delete other people's messages."),
        (MENTION_EVERYONE, "Mention @everyone", "Ping the whole server with @everyone."),
        (SEND_MESSAGES, "Send messages", "Write in text channels."),
        (ATTACH_FILES, "Attach files", "Send pictures, videos and files."),
        (EMBED_LINKS, "Embed links", "Links they post show a preview."),
        (ADD_REACTIONS, "Add reactions", "React to messages with emoji."),
        (CONNECT_VOICE, "Join voice", "Join voice channels."),
        (MOVE_MEMBERS, "Move members", "Drag people in voice to another voice channel."),
        (MUTE_MEMBERS, "Mute members", "Server-mute and server-deafen people in voice."),
        (USE_SOUNDBOARD, "Use soundboard", "Play soundboard sounds in voice channels."),
        (MANAGE_EMOJI, "Manage emoji", "Add, rename and delete the server's emoji and stickers."),
        (MANAGE_SOUNDBOARD, "Manage soundboard", "Add, rename and delete soundboard sounds."),
        (CREATE_INVITES, "Create invites", "Make invite links, see them and revoke them."),
        (KICK_MEMBERS, "Kick members", "Remove people below them. They can come back with a new invite."),
        (BAN_MEMBERS, "Ban members", "Remove people below them for good, and unban them."),
    ];
}

/// A role: a name, an optional colour, and permissions. Higher `position` ranks higher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    /// Name colour (0xRRGGBB) for people whose highest coloured role this is.
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub permissions: u64,
    #[serde(default)]
    pub position: i64,
    /// List people with this role separately in the member list.
    #[serde(default)]
    pub hoist: bool,
}

impl Role {
    pub fn sanitized(mut self) -> Self {
        self.name = self.name.trim().chars().filter(|c| !c.is_control()).take(32).collect();
        if self.name.is_empty() {
            self.name = "new role".into();
        }
        self.color = self.color.map(|c| c & 0xFFFFFF);
        self.permissions &= perms::ALL;
        self
    }
}

/// How many unread messages in a channel mention you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionCount {
    pub channel_id: ChannelId,
    pub count: u32,
}

/// HTTP endpoint for profile pictures and banners: `POST /api/profile-image?kind=avatar|banner`
/// with the (already cropped) picture as the body. `GET /api/profile-image/<id>` downloads one.
pub const PROFILE_IMAGE_PATH: &str = "/api/profile-image";
/// Pixel sizes the app crops profile pictures and banners to (banners are 5:2, like Discord's).
pub const AVATAR_SIZE: u32 = 256;
pub const BANNER_SIZE: (u32, u32) = (680, 272);

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
    /// Roles besides [`EVERYONE_ROLE`] (which everyone has).
    #[serde(default)]
    pub roles: Vec<RoleId>,
    /// Kicked or banned: no longer a member, kept so their old messages still have a name (0.7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// Banned: can't log in or come back until unbanned (0.7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub banned: bool,
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
    /// e.g. "she/her", shown on the profile card.
    pub pronouns: String,
    /// Uploaded profile picture, replacing the letter avatar.
    pub avatar: Option<ProfileImage>,
    /// Uploaded banner picture or GIF, replacing the banner colour.
    pub banner: Option<ProfileImage>,
}

/// A picture uploaded for a profile (see [`PROFILE_IMAGE_PATH`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileImage {
    pub id: String,
    #[serde(default)]
    pub animated: bool,
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
            pronouns: String::new(),
            avatar: None,
            banner: None,
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
        clip(&mut self.pronouns, 40);
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
    /// A group direct message (0.7). Never in [`Ready::channels`]; see [`GroupChannel`].
    Group,
    /// A thread in a text channel (0.7). Never in [`Ready::channels`]; see [`Thread`].
    Thread,
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
    /// The category it's listed under (0.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_id: Option<CategoryId>,
    /// Empty: everyone sees it (or whoever sees its category). Otherwise only people with one of
    /// these roles (and admins) see it, read it, get its events and join it (0.7).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_roles: Vec<RoleId>,
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
    /// Set for videos the app can play (the size above is the video's; the preview is its first frame).
    #[serde(default)]
    pub video: Option<VideoInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    pub duration_ms: u64,
    pub has_audio: bool,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.width.is_some() && self.height.is_some() && self.video.is_none()
    }

    pub fn is_video(&self) -> bool {
        self.video.is_some()
    }
}

/// A link preview under a message, made by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Embed {
    pub url: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Accent colour of the card's left edge (0xRRGGBB).
    #[serde(default)]
    pub color: Option<u32>,
    /// A picture, downloadable like an attachment (`FILES_PATH/<id>`).
    #[serde(default)]
    pub image: Option<Attachment>,
    /// The link itself is a picture or GIF: show it big, without a card.
    #[serde(default)]
    pub media: bool,
    /// For links the server knows more about (0.7): `"youtube"`, `"twitter"` or `"spotify"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// YouTube channel, post author ("Name (@handle)") or artist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_url: Option<String>,
    /// Length of a video or song, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    /// Id for an embedded player: the YouTube video id (`https://www.youtube.com/embed/<id>`),
    /// or a Spotify path like `track/<id>` (`https://open.spotify.com/embed/<path>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_id: Option<String>,
}

impl Embed {
    /// A plain card for `url`, with every optional part empty.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            site_name: None,
            title: None,
            description: None,
            color: None,
            image: None,
            media: false,
            provider: None,
            author: None,
            author_url: None,
            duration_secs: None,
            embed_id: None,
        }
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
    /// Muted by someone else: the server drops their audio.
    #[serde(default)]
    pub server_muted: bool,
    /// Deafened by someone else: the server sends them no audio.
    #[serde(default)]
    pub server_deafened: bool,
    /// When they arrived in this channel (server clock, Unix milliseconds; 0 from older servers).
    #[serde(default)]
    pub joined_at: i64,
    /// Their camera is on (everyone in the call sees it).
    #[serde(default)]
    pub camera: bool,
    /// They're sharing their screen (people who click Watch see it).
    #[serde(default)]
    pub screen: bool,
}

/// Which of someone's streams: their camera, their screen, or their screen's sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoKind {
    Camera,
    Screen,
    /// The sound of a screen share: Opus frames sent as video packets (0.7). It goes to
    /// exactly the people watching the screen share. Apps older than 0.7 never send it and
    /// drop it. Never used in [`ClientMsg::RequestKeyframe`].
    ScreenAudio,
}

impl VideoKind {
    pub fn to_u8(self) -> u8 {
        match self {
            VideoKind::Camera => 1,
            VideoKind::Screen => 2,
            VideoKind::ScreenAudio => 3,
        }
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(VideoKind::Camera),
            2 => Some(VideoKind::Screen),
            3 => Some(VideoKind::ScreenAudio),
            _ => None,
        }
    }
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
    /// Link previews (filled in a moment after the message is sent).
    #[serde(default)]
    pub embeds: Vec<Embed>,
    /// Everyone this message pings: people @mentioned, people with a mentioned role, or
    /// everyone for @everyone. Mentions are written `<@user id>`, `<@&role id>` and `@everyone`.
    #[serde(default)]
    pub mentions: Vec<UserId>,
    /// The message this one replies to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyRef>,
    /// Pinned to its channel.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Ordinary messages, or a note the server posts (a call in a direct message).
    #[serde(default, skip_serializing_if = "MessageKind::is_normal")]
    pub kind: MessageKind,
    /// The thread started from this message, if any (0.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadSummary>,
    /// A poll (0.7). `content` then describes it in words for older apps: show the poll instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<Poll>,
    /// A sticker sent as the message (0.7). Download it from `STICKERS_PATH/<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticker: Option<Sticker>,
    /// An end-to-end encrypted body (0.7, direct messages and groups); `content` is then empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<EncryptedPayload>,
}

/// What a message is. Older apps only see `content`, which describes it in words.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageKind {
    #[default]
    Normal,
    /// A call in a direct message: started by the author, joined by `participants`.
    Call {
        #[serde(default)]
        participants: Vec<UserId>,
        /// Set when everyone has left (unix ms).
        #[serde(default)]
        ended_at: Option<i64>,
    },
    /// The author pinned `message_id` to the channel.
    Pinned { message_id: MessageId },
}

/// Most pinned messages a channel can have.
pub const MAX_PINS: usize = 50;

impl MessageKind {
    pub fn is_normal(&self) -> bool {
        *self == MessageKind::Normal
    }
}

/// A short preview of the message being replied to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyRef {
    pub message_id: MessageId,
    /// `None` if the original message has been deleted.
    pub author_id: Option<UserId>,
    /// The start of the original message (empty if deleted or only files).
    pub snippet: String,
}

/// Longest reply preview, in characters.
pub const REPLY_SNIPPET_LEN: usize = 120;

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
    #[serde(default)]
    pub roles: Vec<Role>,
    /// People you've blocked.
    #[serde(default)]
    pub blocked: Vec<UserId>,
    /// Unread mentions of you, per channel.
    #[serde(default)]
    pub mentions: Vec<MentionCount>,
    /// The server's clock (Unix milliseconds), so times like call lengths can be shown
    /// correctly even if this computer's clock is off. 0 from older servers.
    #[serde(default)]
    pub server_time: i64,
    /// Statuses and activities of the people online (apps older than 0.6 only see `online`).
    #[serde(default)]
    pub presences: Vec<PresenceInfo>,
    /// Channel categories you can see (0.7).
    #[serde(default)]
    pub categories: Vec<Category>,
    /// Active (not archived) threads in channels you can see (0.7).
    #[serde(default)]
    pub threads: Vec<Thread>,
    /// Your group direct messages (0.7).
    #[serde(default)]
    pub groups: Vec<GroupChannel>,
    /// The server's custom emoji, stickers and soundboard sounds (0.7).
    #[serde(default)]
    pub emojis: Vec<CustomEmoji>,
    #[serde(default)]
    pub stickers: Vec<Sticker>,
    #[serde(default)]
    pub sounds: Vec<Sound>,
    /// Published identity keys, for end-to-end encryption (0.7).
    #[serde(default)]
    pub identity_keys: Vec<IdentityKeys>,
    /// The server can make invite links (0.7). Whether you may is the
    /// [`perms::CREATE_INVITES`] permission.
    #[serde(default)]
    pub invite_links: bool,
    /// The address the owner wants invite links to use (`host:port`), if set in the server's
    /// settings. Otherwise use the address you connected to.
    #[serde(default)]
    pub public_address: Option<String>,
}

/// What to search for: every word must appear; the rest narrow it down.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub words: Vec<String>,
    #[serde(default)]
    pub from: Option<UserId>,
    #[serde(default)]
    pub channel_id: Option<ChannelId>,
    #[serde(default)]
    pub has: Option<SearchHas>,
    /// For more results: only messages older than this.
    #[serde(default)]
    pub before: Option<MessageId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchHas {
    Link,
    File,
    Image,
    Video,
}

/// Results per search page.
pub const SEARCH_PAGE: usize = 25;

/// Like Discord's: online, idle (away), do not disturb, or invisible (shown as offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Online,
    Idle,
    DoNotDisturb,
    Invisible,
}

impl Status {
    pub const ALL: [Status; 4] = [Status::Online, Status::Idle, Status::DoNotDisturb, Status::Invisible];

    pub fn label(self) -> &'static str {
        match self {
            Status::Online => "Online",
            Status::Idle => "Idle",
            Status::DoNotDisturb => "Do Not Disturb",
            Status::Invisible => "Invisible",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ActivityKind {
    #[default]
    Playing,
    Watching,
    Listening,
}

impl ActivityKind {
    pub const ALL: [ActivityKind; 3] = [ActivityKind::Playing, ActivityKind::Watching, ActivityKind::Listening];

    /// "Playing", "Watching", "Listening to".
    pub fn verb(self) -> &'static str {
        match self {
            ActivityKind::Playing => "Playing",
            ActivityKind::Watching => "Watching",
            ActivityKind::Listening => "Listening to",
        }
    }
}

impl std::fmt::Display for ActivityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.verb())
    }
}

/// What someone is doing: "Playing Minecraft".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    #[serde(default)]
    pub kind: ActivityKind,
    pub name: String,
}

impl Activity {
    pub fn sanitized(mut self) -> Option<Self> {
        self.name = self.name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
        (!self.name.is_empty()).then_some(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceInfo {
    pub user_id: UserId,
    pub status: Status,
    #[serde(default)]
    pub activity: Option<Activity>,
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
        /// Message in the same channel this one replies to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<MessageId>,
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
        /// Put it in this category (0.7); it then follows the category's roles.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category_id: Option<CategoryId>,
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
        /// Apps from 0.5 encrypt their audio (see [`voice`]); older ones don't send this.
        #[serde(default)]
        encrypted: bool,
    },
    LeaveVoice,
    SetVoiceState {
        muted: bool,
        deafened: bool,
    },
    /// Turn your camera or screen share on or off (in the call you're in).
    SetVideo {
        camera: bool,
        screen: bool,
    },
    /// Start or stop watching someone's screen share.
    WatchStream {
        user_id: UserId,
        watching: bool,
    },
    /// The picture from someone broke up (lost packets): ask them for a fresh keyframe.
    RequestKeyframe {
        user_id: UserId,
        kind: VideoKind,
    },
    /// Move someone who's in voice to another voice channel (needs Move members).
    MoveVoice {
        user_id: UserId,
        channel_id: ChannelId,
    },
    /// Server-mute or server-deafen someone (needs Mute members; kept if they rejoin).
    ModerateVoice {
        user_id: UserId,
        muted: bool,
        deafened: bool,
    },
    /// Needs the manage roles permission (and only roles below your own).
    CreateRole {
        name: String,
    },
    UpdateRole(Role),
    DeleteRole {
        role_id: RoleId,
    },
    /// Replace someone's roles (only roles below your own are changed).
    SetUserRoles {
        user_id: UserId,
        roles: Vec<RoleId>,
    },
    /// Blocked people can't message you, and their messages are hidden for you.
    SetBlocked {
        user_id: UserId,
        blocked: bool,
    },
    /// Remove your uploaded profile picture or banner.
    ClearProfileImage {
        banner: bool,
    },
    /// Search messages in every channel and direct message you can see (newest first).
    Search(SearchQuery),
    /// Pin or unpin a message (Manage messages in server channels; either person in a DM).
    SetPinned {
        message_id: MessageId,
        pinned: bool,
    },
    /// A channel's pinned messages (answered with [`ServerMsg::Pins`]).
    FetchPins {
        channel_id: ChannelId,
    },
    /// Load the messages around one (to jump to a search result).
    FetchAround {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    /// Your status and what you're doing (shown to everyone, unless invisible).
    SetPresence {
        status: Status,
        #[serde(default)]
        activity: Option<Activity>,
    },
    Ping,

    // ---- 0.7: invites and moderation
    /// Make an invite link (needs Create invites). Answered with [`ServerMsg::InviteCreated`].
    CreateInvite {
        /// How long it works, in seconds (`None`: until revoked).
        #[serde(default)]
        max_age_secs: Option<u64>,
        /// How many accounts can be made with it (`None`: no limit).
        #[serde(default)]
        max_uses: Option<u32>,
    },
    /// Answered with [`ServerMsg::Invites`] (needs Create invites).
    ListInvites,
    RevokeInvite {
        token: String,
    },
    /// Remove someone ranked below you (needs Kick members). They're signed out everywhere
    /// and can come back by registering again with their username, password and a new invite.
    KickMember {
        user_id: UserId,
    },
    /// Like kicking, but they can't come back until unbanned (needs Ban members).
    BanMember {
        user_id: UserId,
    },
    UnbanMember {
        user_id: UserId,
    },

    // ---- 0.7: channel categories, ordering and private channels (need Manage channels)
    CreateCategory {
        name: String,
    },
    /// `None` leaves that part as it is.
    UpdateCategory {
        category_id: CategoryId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        allowed_roles: Option<Vec<RoleId>>,
    },
    /// Its channels move to the top level.
    DeleteCategory {
        category_id: CategoryId,
    },
    /// Rename a channel, change its topic, or limit it to roles. `None` leaves that part as it is.
    UpdateChannel {
        channel_id: ChannelId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        allowed_roles: Option<Vec<RoleId>>,
    },
    /// Set positions (and categories) after a drag and drop, all at once. Anything not listed keeps its place.
    ReorderChannels {
        #[serde(default)]
        categories: Vec<CategoryPosition>,
        #[serde(default)]
        channels: Vec<ChannelPosition>,
    },

    // ---- 0.7: threads
    /// Start a thread in a text channel, from one of its messages or on its own (needs Send messages).
    CreateThread {
        channel_id: ChannelId,
        #[serde(default)]
        message_id: Option<MessageId>,
        name: String,
    },
    /// A channel's threads, archived ones too. Answered with [`ServerMsg::Threads`].
    ListThreads {
        channel_id: ChannelId,
    },
    /// Rename or (un)archive a thread (whoever started it, or Manage messages).
    UpdateThread {
        thread_id: ChannelId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        archived: Option<bool>,
    },
    /// Delete a thread and its messages (Manage messages or Manage channels).
    DeleteThread {
        thread_id: ChannelId,
    },

    // ---- 0.7: group direct messages
    /// Start a group with these people (you're added too; 3 to 10 people in all).
    CreateGroup {
        members: Vec<UserId>,
        #[serde(default)]
        name: String,
    },
    /// Only the group's owner can add, remove and rename.
    AddGroupMember {
        channel_id: ChannelId,
        user_id: UserId,
    },
    RemoveGroupMember {
        channel_id: ChannelId,
        user_id: UserId,
    },
    RenameGroup {
        channel_id: ChannelId,
        name: String,
    },
    /// Anyone can leave. If the owner leaves, whoever has been in it longest takes over.
    LeaveGroup {
        channel_id: ChannelId,
    },

    // ---- 0.7: custom emoji, stickers and soundboard (uploads go over HTTP, see [`expressions`])
    RenameEmoji {
        emoji_id: EmojiId,
        name: String,
    },
    DeleteEmoji {
        emoji_id: EmojiId,
    },
    RenameSticker {
        sticker_id: StickerId,
        name: String,
    },
    DeleteSticker {
        sticker_id: StickerId,
    },
    /// Send a sticker as a message.
    SendSticker {
        channel_id: ChannelId,
        sticker_id: StickerId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<MessageId>,
    },
    UpdateSound {
        sound_id: SoundId,
        name: String,
        #[serde(default)]
        emoji: Option<String>,
    },
    DeleteSound {
        sound_id: SoundId,
    },
    /// Play a sound for everyone in your voice channel (needs Use soundboard; rate limited).
    PlaySound {
        sound_id: SoundId,
    },

    // ---- 0.7: polls
    SendPoll {
        channel_id: ChannelId,
        question: String,
        options: Vec<String>,
        #[serde(default)]
        multiple: bool,
        /// How long voting stays open, in seconds (`None`: until the author ends it).
        #[serde(default)]
        duration_secs: Option<u64>,
    },
    /// Vote for (or take back a vote for) one answer. Without `multiple`, voting replaces your other vote.
    Vote {
        message_id: MessageId,
        option: u32,
        voted: bool,
    },
    /// End voting early (the author, or Manage messages).
    EndPoll {
        message_id: MessageId,
    },

    // ---- 0.7: end-to-end encryption (the server only stores and relays)
    /// Publish your identity keys (base64, 32 bytes each). Everyone is told.
    PublishIdentityKeys {
        agreement_key: String,
        signing_key: String,
        #[serde(default)]
        signature: String,
    },
    /// Answered with [`ServerMsg::IdentityKeys`] (people without keys are left out).
    FetchIdentityKeys {
        user_ids: Vec<UserId>,
    },
    /// Send an encrypted message in a direct message or group.
    SendEncrypted {
        channel_id: ChannelId,
        payload: EncryptedPayload,
        #[serde(default)]
        attachments: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<MessageId>,
    },
    /// Replace the body of your own encrypted message.
    EditEncrypted {
        message_id: MessageId,
        payload: EncryptedPayload,
    },
    /// Pass an opaque blob (call key exchange) to the others in your direct-message or group
    /// call: one person (`to`) or everyone else in it (`None`).
    CallSignal {
        #[serde(default)]
        to: Option<UserId>,
        data: String,
    },
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
        /// Online, idle or do-not-disturb (invisible people are just offline to others).
        #[serde(default)]
        status: Status,
        #[serde(default)]
        activity: Option<Activity>,
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
        /// Hex-encoded audio encryption key, when the app asked for encryption.
        #[serde(default)]
        key: Option<String>,
    },
    /// Someone needs a fresh keyframe of your video (sent only to you).
    KeyframeRequest {
        kind: VideoKind,
    },
    /// Someone joined, left (`state: None`) or changed mute/deafen.
    VoiceStateUpdate {
        user_id: UserId,
        state: Option<VoiceState>,
    },
    Error {
        message: String,
    },
    /// A channel's pinned messages, most recently pinned first.
    Pins {
        channel_id: ChannelId,
        messages: Vec<Message>,
    },
    SearchResults {
        /// The query these answer.
        query: SearchQuery,
        results: Vec<Message>,
        /// More results are older than these.
        more: bool,
    },
    /// Messages around a message, replacing what's loaded for that channel.
    Around {
        channel_id: ChannelId,
        message_id: MessageId,
        messages: Vec<Message>,
        reached_start: bool,
    },
    RoleUpdated(Role),
    RoleDeleted {
        role_id: RoleId,
    },
    /// Sent only to you when you block or unblock someone.
    BlockUpdated {
        user_id: UserId,
        blocked: bool,
    },
    Pong,

    // ---- 0.7. Apps older than 0.7 ignore these.
    /// Your new invite (sent only to you).
    InviteCreated(Invite),
    /// Every invite that still works, newest first.
    Invites {
        invites: Vec<Invite>,
    },
    InviteRevoked {
        token: String,
    },
    /// Someone was kicked or banned (a [`ServerMsg::UserUpdated`] with `removed` set follows).
    /// The person themselves gets this and is then disconnected.
    MemberRemoved {
        user_id: UserId,
        banned: bool,
    },
    CategoryUpdated(Category),
    CategoryDeleted {
        category_id: CategoryId,
    },
    /// A channel's name, topic, category, position or roles changed. People who just lost
    /// sight of a channel get [`ServerMsg::ChannelDeleted`]; people who just gained it get
    /// [`ServerMsg::ChannelCreated`].
    ChannelUpdated(Channel),
    ThreadCreated(Thread),
    /// Renamed, (un)archived, or a message was added or deleted.
    ThreadUpdated(Thread),
    ThreadDeleted {
        thread_id: ChannelId,
        parent_id: ChannelId,
    },
    Threads {
        channel_id: ChannelId,
        threads: Vec<Thread>,
    },
    /// A group you're in was made or changed.
    GroupUpdated(GroupChannel),
    /// You left or were removed from a group.
    GroupRemoved {
        channel_id: ChannelId,
    },
    EmojiUpdated(CustomEmoji),
    EmojiDeleted {
        emoji_id: EmojiId,
    },
    StickerUpdated(Sticker),
    StickerDeleted {
        sticker_id: StickerId,
    },
    SoundUpdated(Sound),
    SoundDeleted {
        sound_id: SoundId,
    },
    /// Someone in your voice channel played a sound: play it.
    SoundPlayed {
        channel_id: ChannelId,
        user_id: UserId,
        sound_id: SoundId,
    },
    /// Votes changed or the poll ended.
    PollUpdated {
        channel_id: ChannelId,
        message_id: MessageId,
        poll: Poll,
    },
    IdentityKeysUpdated(IdentityKeys),
    IdentityKeys {
        keys: Vec<IdentityKeys>,
    },
    /// A blob from someone in your call (see [`ClientMsg::CallSignal`]).
    CallSignal {
        channel_id: ChannelId,
        from: UserId,
        data: String,
    },
}

/// Mentions in message text, in the order written: users (`<@id>`), roles (`<@&id>`), and whether @everyone appears.
pub fn parse_mentions(text: &str) -> (Vec<UserId>, Vec<RoleId>, bool) {
    let (mut users, mut roles) = (Vec::new(), Vec::new());
    let mut rest = text;
    while let Some(i) = rest.find("<@") {
        rest = &rest[i + 2..];
        let (is_role, digits) = match rest.strip_prefix('&') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let end = digits.find('>').unwrap_or(0);
        if end > 0
            && let Ok(id) = digits[..end].parse::<i64>()
        {
            let list = if is_role { &mut roles } else { &mut users };
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    let everyone = text.contains("@everyone") || text.contains("@here");
    (users, roles, everyone)
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
/// Custom emoji react with their code (`<:name:id>`), which is why up to 64 bytes are allowed.
pub fn valid_reaction(emoji: &str) -> bool {
    !emoji.is_empty() && emoji.len() <= 64 && !emoji.chars().any(|c| c.is_whitespace() || c.is_control())
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
        let msg = ClientMsg::SendMessage { channel_id: 3, content: "hi".into(), attachments: vec![], reply_to: None };
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
    fn mentions() {
        let (u, r, e) = parse_mentions("hi <@12> and <@&3>, <@12> again <@x> <@> @everyone");
        assert_eq!((u, r, e), (vec![12], vec![3], true));
        assert_eq!(parse_mentions("no pings here"), (vec![], vec![], false));
    }

    #[test]
    fn old_messages_and_profiles_still_read() {
        let m: Message = serde_json::from_str(
            r#"{"id":1,"channel_id":2,"author_id":3,"content":"x","created_at":0,"edited_at":null}"#,
        )
        .unwrap();
        assert!(m.embeds.is_empty() && m.mentions.is_empty());
        let p: Profile = serde_json::from_str(r#"{"display_name":"a"}"#).unwrap();
        assert_eq!(p.pronouns, "");
        assert!(p.avatar.is_none());
    }

    #[test]
    fn additions_in_0_7_stay_compatible() {
        // A 0.6 server's Ready and channel still read, with the new parts empty.
        let ready: Ready = serde_json::from_str(
            r#"{"token":"t","me":1,"community_name":"x","users":[],"online":[],"channels":[
                {"id":1,"name":"general","topic":"","position":0,"kind":"Text"}]}"#,
        )
        .unwrap();
        assert!(ready.categories.is_empty() && ready.groups.is_empty() && !ready.invite_links);
        assert_eq!(ready.channels[0].category_id, None);
        // Plain messages and channels look exactly like they did, so 0.6 apps read them as before.
        let m = Message {
            id: 1,
            channel_id: 2,
            author_id: 3,
            content: "x".into(),
            created_at: 0,
            edited_at: None,
            attachments: vec![],
            reactions: vec![],
            embeds: vec![Embed::new("https://a.example")],
            mentions: vec![],
            reply: None,
            pinned: false,
            kind: MessageKind::Normal,
            thread: None,
            poll: None,
            sticker: None,
            encrypted: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("thread") && !json.contains("poll") && !json.contains("provider"), "{json}");
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), m);
        // The new video kind has its own byte; old kinds keep theirs.
        assert_eq!(VideoKind::from_u8(VideoKind::ScreenAudio.to_u8()), Some(VideoKind::ScreenAudio));
        assert_eq!(VideoKind::Screen.to_u8(), 2);
        // New permissions are within ALL and the old ones haven't moved.
        assert_eq!(perms::ALL & perms::SINCE_0_7, perms::ALL & !0xFFF);
        assert!(perms::LIST.iter().all(|(p, _, _)| perms::ALL & p == *p));
        assert_eq!(perms::LIST.len(), 18);
        // Requests from 0.6 apps still parse.
        let old: ClientMsg = serde_json::from_str(r#"{"op":"CreateChannel","d":{"name":"a","topic":""}}"#).unwrap();
        assert!(matches!(old, ClientMsg::CreateChannel { category_id: None, .. }));
    }

    #[test]
    fn profile_sanitize() {
        let p = Profile { display_name: "x".repeat(100), accent_color: 0xFF123456, ..Default::default() }.sanitized();
        assert_eq!(p.display_name.len(), 32);
        assert_eq!(p.accent_color, 0x123456);
    }
}
