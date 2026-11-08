//! Wire protocol shared by the SynCord server and client.
//!
//! Every frame on the WebSocket is a single JSON object: the client sends
//! [`ClientMsg`] and the server replies / pushes [`ServerMsg`].

use serde::{Deserialize, Serialize};

pub mod voice;

pub type UserId = i64;
pub type ChannelId = i64;
pub type MessageId = i64;
pub type RoleId = i64;

/// The role everyone has. It can't be deleted; its permissions are the defaults.
pub const EVERYONE_ROLE: RoleId = 1;
/// Most roles a server can have.
pub const MAX_ROLES: usize = 50;

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

    pub const ALL: u64 = (1 << 10) - 1;
    /// What the everyone role starts with.
    pub const DEFAULT: u64 = SEND_MESSAGES | ATTACH_FILES | CONNECT_VOICE | EMBED_LINKS | ADD_REACTIONS;

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
    /// Link previews (filled in a moment after the message is sent).
    #[serde(default)]
    pub embeds: Vec<Embed>,
    /// Everyone this message pings: people @mentioned, people with a mentioned role, or
    /// everyone for @everyone. Mentions are written `<@user id>`, `<@&role id>` and `@everyone`.
    #[serde(default)]
    pub mentions: Vec<UserId>,
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
    #[serde(default)]
    pub roles: Vec<Role>,
    /// People you've blocked.
    #[serde(default)]
    pub blocked: Vec<UserId>,
    /// Unread mentions of you, per channel.
    #[serde(default)]
    pub mentions: Vec<MentionCount>,
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
        /// Apps from 0.5 encrypt their audio (see [`voice`]); older ones don't send this.
        #[serde(default)]
        encrypted: bool,
    },
    LeaveVoice,
    SetVoiceState {
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
        /// Hex-encoded audio encryption key, when the app asked for encryption.
        #[serde(default)]
        key: Option<String>,
    },
    /// Someone joined, left (`state: None`) or changed mute/deafen.
    VoiceStateUpdate {
        user_id: UserId,
        state: Option<VoiceState>,
    },
    Error {
        message: String,
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
    fn profile_sanitize() {
        let p = Profile { display_name: "x".repeat(100), accent_color: 0xFF123456, ..Default::default() }.sanitized();
        assert_eq!(p.display_name.len(), 32);
        assert_eq!(p.accent_color, 0x123456);
    }
}
