//! Application state and update logic.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use iced::widget::{Id, operation};
use iced::{Subscription, Task, keyboard};
use syncord_proto::{
    AvatarRing, Channel, ChannelId, ClientMsg, Message as ChatMessage, MessageId, Profile, Ready, ServerMsg, User,
    UserId,
};

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::media::{self, ImageCache, Picture};
use crate::net::{self, Connection};
use crate::settings::Settings;
use crate::theme::{self, AppTheme, Hex, to_u32};
use crate::voice::{self, Link, VoiceEngine};
use syncord_proto::voice::{Token, token_from_hex};
use syncord_proto::{Attachment, DirectChannel};
use syncord_proto::{ChannelKind, VoiceState};

pub const MESSAGES_ID: &str = "messages";
pub const COMPOSER_ID: &str = "composer";
const TYPING_SHOWN_FOR: Duration = Duration::from_secs(6);
const TYPING_RESEND_AFTER: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)] // "Message" here means a chat message, not the iced Message type
pub enum Message {
    // Login screen
    LoginServer(String),
    LoginUsername(String),
    LoginPassword(String),
    LoginInvite(String),
    ToggleRegister,
    SubmitLogin,

    // Connection
    Net(u64, net::Event),
    Reconnect(u64),

    // Chat
    SelectChannel(ChannelId),
    ComposerChanged(String),
    SendMessage,
    LoadOlder,
    HoverMessage(Option<MessageId>),
    StartEdit(MessageId),
    EditChanged(String),
    SubmitEdit,
    CancelEdit,
    DeleteMessage(MessageId),
    /// Right-clicked a message (or pressed its "more" button): show its menu at the pointer.
    OpenMessageMenu(MessageId),
    CloseMessageMenu,
    CopyMessageText(MessageId),
    SelectMessageText(MessageId),
    SelectAction(iced::widget::text_editor::Action),
    StopSelecting,
    MenuReact(MessageId),
    MenuEdit(MessageId),
    MenuDelete(MessageId),
    ToggleMembers,
    DismissNotice,

    // Overlays
    ShowProfile(UserId),
    OpenNewChannel(ChannelKind),
    NewChannelName(String),
    NewChannelTopic(String),
    NewChannelKind(ChannelKind),
    CreateChannel,
    AskDeleteChannel(ChannelId),
    ConfirmDeleteChannel(ChannelId),
    CloseOverlay,

    // Settings
    OpenSettings,
    CloseSettings,
    SettingsTab(SettingsTab),
    Draft(DraftField),
    SaveProfile,
    ResetProfile,
    PickTheme(String),
    ReloadThemes,
    OpenThemeEditor,
    ThemeEditorAction(iced::widget::text_editor::Action),
    ThemeEditorFormat(crate::theme_editor::ThemeFormat),
    SaveThemeEditor,
    CloseThemeEditor,
    TextScale(f32),
    ToggleCompact(bool),
    ToggleLowMemory(bool),
    Logout,

    // Voice
    JoinVoice(ChannelId),
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    VoiceTick,
    InputDevice(String),
    OutputDevice(String),
    InputGain(f32),
    OutputVolume(f32),
    Sensitivity(f32),
    ToggleNoiseSuppression(bool),
    ToggleMicTest,
    UserVolume(UserId, f32),
    ToggleDmSound(bool),
    ToggleServerSound(bool),
    ToggleVoiceSounds(bool),
    SoundVolume(f32),
    TestSound,

    // Pictures and files
    AttachFiles,
    FilesChosen(Vec<PathBuf>),
    PasteShortcut,
    Pasted(Option<Clip>),
    Uploaded(u64, Result<Attachment, String>),
    RemovePending(u64),
    PictureLoaded(String, Result<Arc<Picture>, String>),
    OpenViewer(Attachment),
    SaveAttachment(Attachment),
    SaveTo(Attachment, Option<PathBuf>),
    Saved(u64, Result<String, String>),
    CopyImage(Attachment),
    CopyReady(String, Result<(usize, usize, Arc<Vec<u8>>), String>),
    CopyShortcut,
    /// Redraw upload/download progress.
    TransferTick,
    ToggleFavoriteGif(Attachment),
    SendFavorite(Attachment),
    AnimationTick,
    ToggleAutoplayGifs(bool),

    // Emoji and reactions
    OpenEmojiPicker(PickerTarget),
    /// The GIF button next to the message box.
    OpenGifPicker,
    EmojiSearch(String),
    PickerTab(PickerTab),
    PickEmoji(String),
    PickEmojiSuggestion(String),
    ToggleReaction(MessageId, String),
    /// The search box hasn't changed for a moment: search for GIFs.
    GifSearchDue(String),
    /// The mouse moved onto (true) or off (false) a GIF search result.
    GifHover(String, bool),
    SendGif(String),

    // Updates
    CheckForUpdates,
    UpdateFound(Result<crate::updater::Found, String>),
    UpdateTick,
    UpdateNow,
    UpdateLater,
    OpenUpdatePage,
    DismissUpdate,

    // Direct messages
    ShowHome,
    ShowServer,
    OpenDirect(UserId),
    NewDirect,

    Tick,
    Escape,
}

/// What a picked emoji is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerTarget {
    Composer,
    Reaction(MessageId),
}

/// Which page of the emoji/GIF picker is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerTab {
    /// One of [`crate::emoji::GROUPS`]; `usize::MAX` is "recently used".
    Emoji(usize),
    Gifs,
}

pub struct EmojiPicker {
    pub target: PickerTarget,
    pub search: String,
    pub tab: PickerTab,
}

/// What Ctrl+V found on the clipboard.
#[derive(Debug, Clone)]
pub enum Clip {
    /// A picture (a screenshot, or one copied from a browser), as PNG.
    Image(Vec<u8>),
    /// Files copied in the file manager.
    Files(Vec<PathBuf>),
}

/// A file being uploaded for the next message.
pub struct PendingUpload {
    pub id: u64,
    pub filename: String,
    pub state: UploadState,
    /// Bytes sent so far, out of `size`.
    pub sent: Arc<AtomicU64>,
    pub size: u64,
}

/// A file being saved to disk.
pub struct Download {
    pub id: u64,
    pub filename: String,
    pub done: Arc<AtomicU64>,
    pub size: u64,
}

/// GIF search results in the picker.
#[derive(Default)]
pub struct GifSearch {
    /// The search these results are for ("" is trending).
    pub query: Option<String>,
    pub results: Vec<syncord_proto::GifResult>,
    /// Search waiting for an answer.
    pub waiting: Option<String>,
    pub error: Option<String>,
    pub hovered: Option<String>,
}

pub enum UploadState {
    Uploading,
    Ready(Attachment),
    Failed(String),
}

/// Server channels or the direct-message home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Server,
    Home,
}

/// What the chat area is showing.
pub struct ChannelInfo<'a> {
    pub id: ChannelId,
    pub name: &'a str,
    pub topic: &'a str,
    /// For direct messages: the other person.
    pub direct_with: Option<&'a User>,
}

#[derive(Debug, Clone)]
pub enum DraftField {
    DisplayName(String),
    Status(String),
    Bio(String),
    Glyph(String),
    Accent(String),
    Banner(String),
    Ring(AvatarRing),
    RingColor(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Profile,
    Appearance,
    Voice,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Profile(UserId),
    NewChannel { name: String, topic: String, kind: ChannelKind },
    ConfirmDelete(ChannelId),
    Image(Attachment),
    NewDirect,
}

#[derive(Default)]
pub struct LoginForm {
    pub server: String,
    pub username: String,
    pub password: String,
    pub invite: String,
    pub register: bool,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Default)]
pub struct ChannelState {
    pub messages: Vec<ChatMessage>,
    pub loaded: bool,
    pub loading: bool,
    pub reached_start: bool,
}

pub struct Session {
    pub me: UserId,
    pub community: String,
    pub users: HashMap<UserId, User>,
    pub online: HashSet<UserId>,
    pub channels: Vec<Channel>,
    pub current: Option<ChannelId>,
    pub chans: HashMap<ChannelId, ChannelState>,
    pub typing: HashMap<ChannelId, HashMap<UserId, Instant>>,
    /// Everyone currently in a voice channel.
    pub voice: HashMap<UserId, VoiceState>,
    pub view: View,
    /// Your direct conversations, most recently active first.
    pub directs: Vec<DirectChannel>,
    /// Newest message read, per channel.
    pub read: HashMap<ChannelId, MessageId>,
    /// Newest message known, per channel.
    pub latest: HashMap<ChannelId, MessageId>,
    /// Server channel to go back to when leaving the DM home.
    pub last_server_channel: Option<ChannelId>,
    pub server_version: String,
    /// GIF search service the server offers, if any.
    pub gif_provider: Option<String>,
    /// Largest upload the server accepts, in MB (0 if it doesn't say).
    pub max_upload_mb: u64,
}

impl Session {
    pub fn me(&self) -> Option<&User> {
        self.users.get(&self.me)
    }

    pub fn direct(&self, id: ChannelId) -> Option<&DirectChannel> {
        self.directs.iter().find(|d| d.id == id)
    }

    /// Name and topic of the channel being shown, for server channels and DMs alike.
    pub fn current_info(&self) -> Option<ChannelInfo<'_>> {
        let id = self.current?;
        if let Some(c) = self.channels.iter().find(|c| c.id == id) {
            return Some(ChannelInfo { id, name: &c.name, topic: &c.topic, direct_with: None });
        }
        let d = self.direct(id)?;
        let user = self.users.get(&d.user_id)?;
        Some(ChannelInfo { id, name: user.name(), topic: &user.profile.status_text, direct_with: Some(user) })
    }

    pub fn is_unread(&self, channel: ChannelId) -> bool {
        match (self.latest.get(&channel), self.read.get(&channel)) {
            (Some(latest), Some(read)) => latest > read,
            (Some(_), None) => true,
            _ => false,
        }
    }

    pub fn unread_directs(&self) -> usize {
        self.directs.iter().filter(|d| self.is_unread(d.id)).count()
    }

    pub fn first_text_channel(&self) -> Option<ChannelId> {
        self.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id)
    }

    pub fn is_owner(&self) -> bool {
        self.me().is_some_and(|u| u.is_owner)
    }
}

/// Profile being edited in settings; colours are kept as text so half-typed hex values work.
#[derive(Debug, Clone, Default)]
pub struct ProfileDraft {
    pub display_name: String,
    pub status: String,
    pub bio: String,
    pub glyph: String,
    pub accent: String,
    pub banner: String,
    pub ring: AvatarRing,
    pub ring_color: String,
}

impl ProfileDraft {
    pub fn from_profile(p: &Profile) -> Self {
        Self {
            display_name: p.display_name.clone(),
            status: p.status_text.clone(),
            bio: p.bio.clone(),
            glyph: p.avatar_glyph.clone(),
            accent: format!("#{:06X}", p.accent_color),
            banner: format!("#{:06X}", p.banner_color),
            ring: p.avatar_ring,
            ring_color: format!("#{:06X}", p.ring_color),
        }
    }

    /// The profile as it would look if saved, falling back to `base` for invalid colours.
    pub fn to_profile(&self, base: &Profile) -> Profile {
        let color = |s: &str, fallback: u32| Hex::parse(s).map(to_u32).unwrap_or(fallback);
        Profile {
            display_name: self.display_name.clone(),
            status_text: self.status.clone(),
            bio: self.bio.clone(),
            avatar_glyph: self.glyph.clone(),
            accent_color: color(&self.accent, base.accent_color),
            banner_color: color(&self.banner, base.banner_color),
            avatar_ring: self.ring,
            ring_color: color(&self.ring_color, base.ring_color),
        }
        .sanitized()
    }
}

/// Label for "use the system's default device" in the device pickers.
pub const DEFAULT_DEVICE: &str = "Default";

/// The voice channel we're connected to.
pub struct ActiveVoice {
    pub channel_id: ChannelId,
    pub ssrc: u32,
    token: Token,
    udp_port: u16,
    pub engine: VoiceEngine,
}

pub struct App {
    pub settings: Settings,
    pub themes: Vec<AppTheme>,
    pub theme_errors: Vec<String>,
    pub theme: AppTheme,
    pub theme_editor: Option<crate::theme_editor::ThemeEditor>,

    generation: u64,
    pub(crate) conn: Option<Connection>,
    voice_sink: crate::voice::VoiceSink,
    pending_auth: Option<ClientMsg>,
    reconnect_attempts: u32,
    pub reconnecting: bool,

    pub login: LoginForm,
    pub session: Option<Session>,

    pub composer: String,
    last_typing_sent: Option<Instant>,
    pub hovered: Option<MessageId>,
    pub editing: Option<(MessageId, String)>,
    /// Message whose right-click menu is open, and where (window coordinates).
    pub message_menu: Option<(MessageId, iced::Point)>,
    /// Message whose text is being selected.
    pub selecting: Option<crate::app_messages::Selecting>,
    pub overlay: Option<Overlay>,
    pub settings_tab: Option<SettingsTab>,
    pub draft: ProfileDraft,
    pub notice: Option<String>,

    pub voice: Option<ActiveVoice>,
    pub voice_joining: Option<ChannelId>,
    /// Voice channel to rejoin after a reconnect.
    rejoin_voice: Option<ChannelId>,
    pub muted: bool,
    pub deafened: bool,
    pub voice_status: voice::Status,
    pub speaking: HashSet<UserId>,
    pub mic_test: Option<VoiceEngine>,
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,

    pub http: reqwest::Client,
    pub images: ImageCache,
    /// Start of the GIF animation clock, and the latest tick.
    pub anim_epoch: Instant,
    pub now: Instant,
    pub pending: Vec<PendingUpload>,
    pub(crate) pending_channel: Option<ChannelId>,
    pub(crate) next_upload_id: u64,
    pub emoji_picker: Option<EmojiPicker>,
    pub gifs: GifSearch,
    pub downloads: Vec<Download>,
    pub(crate) next_download_id: u64,
    /// Kept open so pictures copied from SynCord stay on the clipboard (needed on Linux).
    pub(crate) clipboard: Option<arboard::Clipboard>,
    /// Picture being copied from the viewer, and whether it's done.
    pub copying: Option<(String, bool)>,
    pub(crate) pending_direct: Option<UserId>,
    pub update: crate::updater::Stage,
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let settings = Settings::load();
        let (themes, theme_errors) = theme::load_all();
        let theme = themes.iter().find(|t| t.name == settings.theme).cloned().unwrap_or_else(|| themes[0].clone());

        let mut app = Self {
            login: LoginForm {
                server: settings.server.clone(),
                username: settings.username.clone(),
                ..Default::default()
            },
            settings,
            themes,
            theme_errors,
            theme,
            theme_editor: None,
            generation: 0,
            conn: None,
            voice_sink: Default::default(),
            pending_auth: None,
            reconnect_attempts: 0,
            reconnecting: false,
            session: None,
            composer: String::new(),
            last_typing_sent: None,
            hovered: None,
            editing: None,
            message_menu: None,
            selecting: None,
            overlay: None,
            settings_tab: None,
            draft: ProfileDraft::default(),
            notice: None,
            voice: None,
            voice_joining: None,
            rejoin_voice: None,
            muted: false,
            deafened: false,
            voice_status: voice::Status::default(),
            speaking: HashSet::new(),
            mic_test: None,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            http: media::client(),
            images: ImageCache::default(),
            anim_epoch: Instant::now(),
            now: Instant::now(),
            pending: Vec::new(),
            pending_channel: None,
            next_upload_id: 1,
            emoji_picker: None,
            gifs: GifSearch::default(),
            downloads: Vec::new(),
            next_download_id: 1,
            clipboard: None,
            copying: None,
            pending_direct: None,
            update: crate::updater::Stage::Checking,
        };

        // Log straight back in if we have a saved session.
        let task = match (&app.settings.token, app.settings.server.is_empty()) {
            (Some(token), false) => {
                let msg = ClientMsg::Resume { token: token.clone() };
                app.login.busy = true;
                app.connect(msg)
            }
            _ => Task::none(),
        };
        // Look for updates shortly after starting.
        let check = Task::perform(
            async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                crate::updater::check_and_download().await
            },
            Message::UpdateFound,
        );
        (app, Task::batch([task, check]))
    }

    pub fn title(&self) -> String {
        match self.session.as_ref().and_then(|s| s.current_info()) {
            Some(c) if c.direct_with.is_some() => format!("@{} - SynCord", c.name),
            Some(c) => format!("#{} - SynCord", c.name),
            None => "SynCord".into(),
        }
    }

    pub fn iced_theme(&self) -> iced::Theme {
        self.theme.iced()
    }

    /// Base font size after the user's scaling preference.
    pub fn fs(&self) -> f32 {
        (self.theme.font_size * self.settings.text_scale).clamp(9.0, 30.0)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // Escape (even while typing), files dropped on the window, and Ctrl/Cmd+V for pasting pictures.
        let mut subs = vec![iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) => Some(Message::Escape),
            iced::Event::Window(iced::window::Event::FileDropped(path)) => Some(Message::FilesChosen(vec![path])),
            // Remembered for placing the message menu; not a message, so moving the mouse costs nothing.
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                crate::app_messages::remember_cursor(position);
                None
            }
            // Match the letter or the key's position, so it works with any keyboard layout.
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, physical_key, modifiers, .. })
                if modifiers.command() && !modifiers.alt() =>
            {
                let is = |letter: &str, code: keyboard::key::Code| {
                    matches!(&key, keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case(letter))
                        || physical_key == keyboard::key::Physical::Code(code)
                };
                if is("v", keyboard::key::Code::KeyV) {
                    Some(Message::PasteShortcut)
                } else if is("c", keyboard::key::Code::KeyC) {
                    Some(Message::CopyShortcut)
                } else {
                    None
                }
            }
            _ => None,
        })];
        let someone_typing = self.session.as_ref().is_some_and(|s| s.typing.values().any(|m| !m.is_empty()));
        if someone_typing {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        if self.voice.is_some() || self.mic_test.is_some() {
            // Refresh speaking indicators and the mic meter.
            subs.push(iced::time::every(Duration::from_millis(100)).map(|_| Message::VoiceTick));
        }
        {
            use crate::updater::{CHECK_EVERY, Stage};
            match self.update {
                Stage::Ready { .. } => {
                    subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::UpdateTick))
                }
                Stage::UpToDate | Stage::Failed(_) => {
                    subs.push(iced::time::every(CHECK_EVERY).map(|_| Message::CheckForUpdates))
                }
                _ => {}
            }
        }
        if !self.downloads.is_empty() || self.pending.iter().any(|p| matches!(p.state, UploadState::Uploading)) {
            subs.push(iced::time::every(Duration::from_millis(250)).map(|_| Message::TransferTick));
        }
        if self.wants_animation() {
            subs.push(iced::time::every(Duration::from_millis(50)).map(|_| Message::AnimationTick));
        }
        Subscription::batch(subs)
    }

    fn connect(&mut self, auth: ClientMsg) -> Task<Message> {
        self.generation += 1;
        self.conn = None;
        self.pending_auth = Some(auth);
        let generation = self.generation;
        let url = net::server_url(&self.settings.server);
        Task::run(net::connect(url, self.voice_sink.clone()), move |event| Message::Net(generation, event))
    }

    pub(crate) fn send(&self, msg: ClientMsg) {
        if let Some(conn) = &self.conn {
            conn.send(msg);
        }
    }

    /// Scroll the messages to the newest one. The list is anchored to the bottom,
    /// so offsets count from the bottom and "start" is the newest message.
    fn snap_to_end() -> Task<Message> {
        operation::snap_to(Id::new(MESSAGES_ID), operation::RelativeOffset::START)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::LoginServer(s) => self.login.server = s,
            Message::LoginUsername(s) => self.login.username = s,
            Message::LoginPassword(s) => self.login.password = s,
            Message::LoginInvite(s) => self.login.invite = s,
            Message::ToggleRegister => {
                self.login.register = !self.login.register;
                self.login.error = None;
            }
            Message::SubmitLogin => {
                let f = &self.login;
                if f.busy {
                    return Task::none();
                }
                if f.server.trim().is_empty() || f.username.trim().is_empty() || f.password.is_empty() {
                    self.login.error = Some("Fill in the server address, username and password.".into());
                    return Task::none();
                }
                let auth = if f.register {
                    ClientMsg::Register {
                        username: f.username.trim().into(),
                        password: f.password.clone(),
                        invite: f.invite.trim().into(),
                    }
                } else {
                    ClientMsg::Login { username: f.username.trim().into(), password: f.password.clone() }
                };
                self.settings.server = f.server.trim().into();
                self.settings.username = f.username.trim().into();
                self.settings.save();
                self.login.busy = true;
                self.login.error = None;
                return self.connect(auth);
            }

            Message::Net(generation, event) => {
                if generation != self.generation {
                    return Task::none(); // from a connection we've abandoned
                }
                return self.on_net(event);
            }
            Message::Reconnect(generation) => {
                if generation == self.generation
                    && self.session.is_some()
                    && let Some(token) = self.settings.token.clone()
                {
                    return self.connect(ClientMsg::Resume { token });
                }
            }

            Message::SelectChannel(id) => return self.select_channel(id),
            Message::ComposerChanged(s) => {
                let typing_now = !s.trim().is_empty();
                self.composer = s;
                if typing_now
                    && self.last_typing_sent.is_none_or(|t| t.elapsed() > TYPING_RESEND_AFTER)
                    && let Some(channel_id) = self.session.as_ref().and_then(|s| s.current)
                {
                    self.send(ClientMsg::Typing { channel_id });
                    self.last_typing_sent = Some(Instant::now());
                }
            }
            Message::SendMessage => {
                // Enter while an emoji suggestion is showing picks it instead of sending.
                if let Some((emoji, _)) = self.emoji_suggestions().first() {
                    let emoji = emoji.as_str().to_string();
                    self.accept_suggestion(&emoji);
                    return Task::none();
                }
                let content = crate::emoji::replace_shortcodes(self.composer.trim());
                if self.pending.iter().any(|p| matches!(p.state, UploadState::Uploading)) {
                    self.notice = Some("Still uploading. Press Enter again when the files are ready.".into());
                    return Task::none();
                }
                let attachments: Vec<String> = self
                    .pending
                    .iter()
                    .filter_map(|p| match &p.state {
                        UploadState::Ready(a) => Some(a.id.clone()),
                        _ => None,
                    })
                    .collect();
                if content.is_empty() && attachments.is_empty() {
                    return Task::none();
                }
                if let Some(channel_id) = self.session.as_ref().and_then(|s| s.current) {
                    if self.conn.is_none() {
                        self.notice = Some("Not connected right now; your message wasn't sent.".into());
                        return Task::none();
                    }
                    self.send(ClientMsg::SendMessage { channel_id, content, attachments });
                    self.composer.clear();
                    self.pending.clear();
                    self.emoji_picker = None;
                    self.last_typing_sent = None;
                    return Self::snap_to_end();
                }
            }
            Message::LoadOlder => {
                if let Some(s) = &mut self.session
                    && let Some(id) = s.current
                {
                    let st = s.chans.entry(id).or_default();
                    if !st.loading && !st.reached_start {
                        st.loading = true;
                        let before = st.messages.first().map(|m| m.id);
                        self.send(ClientMsg::FetchHistory { channel_id: id, before });
                    }
                }
            }
            Message::HoverMessage(id) => self.hovered = id,
            Message::StartEdit(id) => {
                let content = self
                    .session
                    .as_ref()
                    .and_then(|s| s.current.and_then(|c| s.chans.get(&c)))
                    .and_then(|st| st.messages.iter().find(|m| m.id == id))
                    .map(|m| m.content.clone());
                if let Some(content) = content {
                    self.editing = Some((id, content));
                    return operation::focus(Id::new("edit"));
                }
            }
            Message::EditChanged(s) => {
                if let Some((_, text)) = &mut self.editing {
                    *text = s;
                }
            }
            Message::SubmitEdit => {
                if let Some((message_id, content)) = self.editing.take() {
                    if content.trim().is_empty() {
                        self.send(ClientMsg::DeleteMessage { message_id });
                    } else {
                        self.send(ClientMsg::EditMessage { message_id, content });
                    }
                }
            }
            Message::CancelEdit => self.editing = None,
            Message::DeleteMessage(message_id) => self.send(ClientMsg::DeleteMessage { message_id }),
            Message::ToggleMembers => {
                self.settings.show_members = !self.settings.show_members;
                self.settings.save();
            }
            Message::DismissNotice => self.notice = None,

            Message::ShowProfile(id) => self.overlay = Some(Overlay::Profile(id)),
            Message::OpenNewChannel(kind) => {
                self.overlay = Some(Overlay::NewChannel { name: String::new(), topic: String::new(), kind });
                return operation::focus(Id::new("new-channel"));
            }
            Message::NewChannelName(s) => {
                if let Some(Overlay::NewChannel { name, .. }) = &mut self.overlay {
                    *name = s;
                }
            }
            Message::NewChannelTopic(s) => {
                if let Some(Overlay::NewChannel { topic, .. }) = &mut self.overlay {
                    *topic = s;
                }
            }
            Message::NewChannelKind(k) => {
                if let Some(Overlay::NewChannel { kind, .. }) = &mut self.overlay {
                    *kind = k;
                }
            }
            Message::CreateChannel => {
                if let Some(Overlay::NewChannel { name, topic, kind }) = self.overlay.take() {
                    self.send(ClientMsg::CreateChannel { name, topic, kind });
                }
            }
            Message::AskDeleteChannel(id) => self.overlay = Some(Overlay::ConfirmDelete(id)),
            Message::ConfirmDeleteChannel(channel_id) => {
                self.overlay = None;
                self.send(ClientMsg::DeleteChannel { channel_id });
            }
            Message::CloseOverlay => self.close_overlay(),

            Message::OpenSettings => {
                if let Some(me) = self.session.as_ref().and_then(|s| s.me()) {
                    self.draft = ProfileDraft::from_profile(&me.profile);
                }
                self.settings_tab = Some(SettingsTab::Profile);
                self.overlay = None;
            }
            Message::CloseSettings => {
                // Leaving settings abandons unsaved theme edits.
                if let Some(ed) = self.theme_editor.take() {
                    self.theme = ed.original;
                }
                self.settings_tab = None;
                self.mic_test = None;
            }
            Message::SettingsTab(tab) => {
                if tab == SettingsTab::Voice {
                    self.input_devices = voice::devices::input_names();
                    self.output_devices = voice::devices::output_names();
                } else {
                    self.mic_test = None;
                }
                self.settings_tab = Some(tab);
            }
            Message::Draft(field) => {
                let d = &mut self.draft;
                match field {
                    DraftField::DisplayName(s) => d.display_name = s,
                    DraftField::Status(s) => d.status = s,
                    DraftField::Bio(s) => d.bio = s,
                    DraftField::Glyph(s) => d.glyph = s.chars().take(2).collect(),
                    DraftField::Accent(s) => d.accent = s,
                    DraftField::Banner(s) => d.banner = s,
                    DraftField::Ring(r) => d.ring = r,
                    DraftField::RingColor(s) => d.ring_color = s,
                }
            }
            Message::SaveProfile => {
                if let Some(me) = self.session.as_ref().and_then(|s| s.me()) {
                    let profile = self.draft.to_profile(&me.profile);
                    self.send(ClientMsg::UpdateProfile { profile });
                    self.notice = None;
                }
            }
            Message::ResetProfile => {
                if let Some(me) = self.session.as_ref().and_then(|s| s.me()) {
                    self.draft = ProfileDraft::from_profile(&me.profile);
                }
            }
            Message::PickTheme(name) => {
                // Picking a theme abandons unsaved edits.
                self.theme_editor = None;
                if let Some(t) = self.themes.iter().find(|t| t.name == name) {
                    self.theme = t.clone();
                    self.settings.theme = name;
                    self.settings.save();
                }
            }
            Message::ReloadThemes => {
                self.theme_editor = None;
                let (themes, errors) = theme::load_all();
                self.themes = themes;
                self.theme_errors = errors;
                if let Some(t) = self.themes.iter().find(|t| t.name == self.settings.theme) {
                    self.theme = t.clone();
                }
            }
            Message::TextScale(v) => {
                self.settings.text_scale = v;
                self.settings.save();
            }
            Message::ToggleCompact(v) => {
                self.settings.compact = v;
                self.settings.save();
            }
            Message::ToggleLowMemory(v) => {
                self.settings.low_memory = v;
                self.settings.save();
            }
            Message::Logout => {
                self.send(ClientMsg::Logout);
                self.end_session(None);
            }

            Message::JoinVoice(id) => {
                if self.voice.as_ref().is_some_and(|v| v.channel_id == id) || self.voice_joining == Some(id) {
                    return Task::none();
                }
                self.mic_test = None;
                self.stop_voice();
                self.voice_joining = Some(id);
                self.send(ClientMsg::JoinVoice { channel_id: id });
            }
            Message::LeaveVoice => {
                if self.voice.is_some() && self.settings.voice_sounds {
                    self.settings.play(crate::sounds::Sound::Leave);
                }
                self.send(ClientMsg::LeaveVoice);
                self.voice_joining = None;
                self.rejoin_voice = None;
                self.stop_voice();
            }
            Message::ToggleMute => {
                if self.deafened {
                    // Unmuting while deafened turns both off, like other voice apps.
                    self.deafened = false;
                    self.muted = false;
                } else {
                    self.muted = !self.muted;
                }
                self.apply_mute_state();
            }
            Message::ToggleDeafen => {
                self.deafened = !self.deafened;
                self.apply_mute_state();
            }
            Message::VoiceTick => {
                let status = match (&self.voice, &self.mic_test) {
                    (Some(v), _) => v.engine.status(),
                    (None, Some(t)) => t.status(),
                    (None, None) => return Task::none(),
                };
                let mut speaking = HashSet::new();
                if let (Some(v), Some(s)) = (&self.voice, &self.session) {
                    for st in s.voice.values() {
                        if st.channel_id == v.channel_id && status.speaking.contains(&st.ssrc) {
                            speaking.insert(st.user_id);
                        }
                    }
                    if status.transmitting {
                        speaking.insert(s.me);
                    }
                }
                self.speaking = speaking;
                self.voice_status = status;
            }
            Message::InputDevice(name) => {
                self.settings.input_device = (name != DEFAULT_DEVICE).then_some(name);
                self.settings.save();
                self.restart_audio();
            }
            Message::OutputDevice(name) => {
                self.settings.output_device = (name != DEFAULT_DEVICE).then_some(name);
                self.settings.save();
                self.restart_audio();
            }
            Message::InputGain(v) => {
                self.settings.input_gain = v;
                self.save_and_apply_voice_config();
            }
            Message::OutputVolume(v) => {
                self.settings.output_volume = v;
                self.save_and_apply_voice_config();
            }
            Message::Sensitivity(v) => {
                self.settings.input_sensitivity = v;
                self.save_and_apply_voice_config();
            }
            Message::ToggleNoiseSuppression(v) => {
                self.settings.noise_suppression = v;
                self.save_and_apply_voice_config();
            }
            Message::ToggleDmSound(v) => {
                self.settings.dm_sound = v;
                self.settings.save();
            }
            Message::ToggleServerSound(v) => {
                self.settings.server_sound = v;
                self.settings.save();
            }
            Message::ToggleVoiceSounds(v) => {
                self.settings.voice_sounds = v;
                self.settings.save();
            }
            Message::SoundVolume(v) => {
                self.settings.sound_volume = v;
                self.settings.save();
            }
            Message::TestSound => self.settings.play(crate::sounds::Sound::Message),
            Message::ToggleMicTest => {
                if self.mic_test.take().is_none() && self.voice.is_none() {
                    match VoiceEngine::start(self.settings.voice_config(), Link::Loopback, false, false) {
                        Ok(engine) => {
                            self.notice = engine.status().problem;
                            self.mic_test = Some(engine);
                        }
                        Err(e) => self.notice = Some(format!("Mic test failed: {e}")),
                    }
                }
                if self.mic_test.is_none() {
                    self.voice_status = voice::Status::default();
                }
            }
            Message::UserVolume(user, volume) => {
                self.settings.user_volumes.insert(user.to_string(), volume);
                self.settings.save();
                if let (Some(v), Some(st)) = (&self.voice, self.session.as_ref().and_then(|s| s.voice.get(&user))) {
                    v.engine.set_speaker_volume(st.ssrc, volume);
                }
            }

            Message::AttachFiles
            | Message::FilesChosen(_)
            | Message::PasteShortcut
            | Message::Pasted(_)
            | Message::Uploaded(..)
            | Message::RemovePending(_)
            | Message::PictureLoaded(..)
            | Message::OpenViewer(_)
            | Message::SaveAttachment(_)
            | Message::SaveTo(..)
            | Message::Saved(..)
            | Message::CopyImage(_)
            | Message::CopyReady(..)
            | Message::CopyShortcut
            | Message::TransferTick
            | Message::ToggleFavoriteGif(_)
            | Message::SendFavorite(_)
            | Message::AnimationTick
            | Message::ToggleAutoplayGifs(_) => return self.update_media(message),
            Message::OpenEmojiPicker(_)
            | Message::OpenGifPicker
            | Message::EmojiSearch(_)
            | Message::PickerTab(_)
            | Message::PickEmoji(_)
            | Message::PickEmojiSuggestion(_)
            | Message::ToggleReaction(..)
            | Message::GifSearchDue(_)
            | Message::GifHover(..)
            | Message::SendGif(_)
            | Message::ShowHome
            | Message::ShowServer
            | Message::OpenDirect(_)
            | Message::NewDirect => return self.update_social(message),

            Message::OpenMessageMenu(_)
            | Message::CloseMessageMenu
            | Message::CopyMessageText(_)
            | Message::SelectMessageText(_)
            | Message::SelectAction(_)
            | Message::StopSelecting
            | Message::MenuReact(_)
            | Message::MenuEdit(_)
            | Message::MenuDelete(_) => return self.update_messages(message),
            Message::OpenThemeEditor
            | Message::ThemeEditorAction(_)
            | Message::ThemeEditorFormat(_)
            | Message::SaveThemeEditor
            | Message::CloseThemeEditor => return self.update_theme_editor(message),

            Message::CheckForUpdates
            | Message::UpdateFound(_)
            | Message::UpdateTick
            | Message::UpdateNow
            | Message::UpdateLater
            | Message::OpenUpdatePage
            | Message::DismissUpdate => return self.update_updates(message),

            Message::Tick => {
                if let Some(s) = &mut self.session {
                    for users in s.typing.values_mut() {
                        users.retain(|_, at| at.elapsed() < TYPING_SHOWN_FOR);
                    }
                }
            }
            Message::Escape => {
                if self.message_menu.is_some() {
                    self.message_menu = None;
                } else if self.selecting.is_some() {
                    self.selecting = None;
                } else if self.emoji_picker.is_some() {
                    self.emoji_picker = None;
                } else if self.overlay.is_some() {
                    self.close_overlay();
                } else if self.editing.is_some() {
                    self.editing = None;
                } else if self.theme_editor.is_some() {
                    return self.update_theme_editor(Message::CloseThemeEditor);
                } else if self.settings_tab.is_some() {
                    self.settings_tab = None;
                }
            }
        }
        Task::none()
    }

    fn update_updates(&mut self, message: Message) -> Task<Message> {
        use crate::updater::{self, COUNTDOWN, Found, Stage};
        let countdown = || Instant::now() + Duration::from_secs(COUNTDOWN);
        match message {
            Message::CheckForUpdates => {
                if matches!(self.update, Stage::Checking | Stage::Ready { .. } | Stage::Installing(_)) {
                    return Task::none();
                }
                self.update = Stage::Checking;
                return Task::perform(updater::check_and_download(), Message::UpdateFound);
            }
            Message::UpdateFound(result) => {
                self.update = match result {
                    Ok(Found::UpToDate) => Stage::UpToDate,
                    Ok(Found::Ready { version, notes, file }) => {
                        Stage::Ready { version, notes, file, deadline: countdown() }
                    }
                    Ok(Found::Manual { version, page }) => Stage::Manual { version, page, dismissed: false },
                    Err(e) => {
                        eprintln!("update check failed: {e}");
                        Stage::Failed(e)
                    }
                };
            }
            Message::UpdateTick => {
                let in_call = self.voice.is_some() || self.voice_joining.is_some();
                if let Stage::Ready { deadline, .. } = &mut self.update {
                    if in_call {
                        // Never interrupt a call: start counting again once it's over.
                        *deadline = countdown();
                    } else if Instant::now() >= *deadline {
                        return self.install_update();
                    }
                }
            }
            Message::UpdateNow => return self.install_update(),
            Message::UpdateLater => {
                if let Stage::Ready { version, .. } = &self.update {
                    self.update = Stage::Postponed { version: version.clone() };
                }
            }
            Message::OpenUpdatePage => {
                if let Stage::Manual { page, .. } = &self.update {
                    updater::open_in_browser(page);
                }
            }
            Message::DismissUpdate => {
                if let Stage::Manual { dismissed, .. } = &mut self.update {
                    *dismissed = true;
                }
            }
            _ => {}
        }
        Task::none()
    }

    fn install_update(&mut self) -> Task<Message> {
        use crate::updater::{self, Stage};
        let Stage::Ready { version, file, .. } = &self.update else { return Task::none() };
        let (version, file) = (version.clone(), file.clone());
        self.update = Stage::Installing(version.clone());
        match updater::install(&file) {
            Ok(()) => {
                // Settings are saved as they change; just make sure, then hand over to the new version.
                self.settings.save();
                iced::exit()
            }
            Err(e) => {
                self.notice = Some(format!("Couldn't install SynCord {version}: {e}"));
                self.update = Stage::Failed(e);
                Task::none()
            }
        }
    }

    fn stop_voice(&mut self) {
        self.voice = None;
        *self.voice_sink.lock().unwrap() = None;
        self.speaking.clear();
        self.voice_status = voice::Status::default();
    }

    fn start_engine(&self, token: Token, udp_port: u16) -> Result<VoiceEngine, String> {
        let conn = self.conn.clone().ok_or("Not connected")?;
        let host = net::host_of(&net::server_url(&self.settings.server)).unwrap_or_default();
        let link = Link::Server { host, udp_port, token, conn };
        VoiceEngine::start(self.settings.voice_config(), link, self.muted, self.deafened)
    }

    /// Hook a freshly started engine up to the network and per-person volumes.
    fn install_engine(&mut self, channel_id: ChannelId, ssrc: u32, token: Token, udp_port: u16, engine: VoiceEngine) {
        *self.voice_sink.lock().unwrap() = Some(engine.mixer());
        if let Some(s) = &self.session {
            for st in s.voice.values() {
                engine.set_speaker_volume(st.ssrc, self.settings.user_volume(st.user_id));
            }
        }
        if let Some(problem) = engine.status().problem {
            self.notice = Some(problem);
        }
        self.voice = Some(ActiveVoice { channel_id, ssrc, token, udp_port, engine });
    }

    fn apply_mute_state(&self) {
        if let Some(v) = &self.voice {
            v.engine.set_muted(self.muted);
            v.engine.set_deafened(self.deafened);
            self.send(ClientMsg::SetVoiceState { muted: self.muted, deafened: self.deafened });
        }
    }

    fn save_and_apply_voice_config(&mut self) {
        self.settings.save();
        let config = self.settings.voice_config();
        if let Some(v) = &self.voice {
            v.engine.set_config(&config);
        }
        if let Some(t) = &self.mic_test {
            t.set_config(&config);
        }
    }

    /// Reopen audio devices after the user picks a different microphone or speaker.
    fn restart_audio(&mut self) {
        if self.mic_test.take().is_some() {
            return self.restart_mic_test();
        }
        if let Some(v) = self.voice.take() {
            let (channel_id, ssrc, token, udp_port) = (v.channel_id, v.ssrc, v.token, v.udp_port);
            drop(v); // release the old devices first
            match self.start_engine(token, udp_port) {
                Ok(engine) => self.install_engine(channel_id, ssrc, token, udp_port, engine),
                Err(e) => {
                    self.notice = Some(format!("Couldn't switch audio device: {e}"));
                    self.send(ClientMsg::LeaveVoice);
                    self.stop_voice();
                }
            }
        }
    }

    fn restart_mic_test(&mut self) {
        match VoiceEngine::start(self.settings.voice_config(), Link::Loopback, false, false) {
            Ok(engine) => self.mic_test = Some(engine),
            Err(e) => self.notice = Some(format!("Mic test failed: {e}")),
        }
    }

    fn on_voice(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::VoiceReady { channel_id, ssrc, token, udp_port } => {
                // Ignore answers to joins we've since cancelled or replaced.
                if self.voice_joining != Some(channel_id) {
                    return;
                }
                self.voice_joining = None;
                let Some(token) = token_from_hex(&token) else { return };
                match self.start_engine(token, udp_port) {
                    Ok(engine) => {
                        self.install_engine(channel_id, ssrc, token, udp_port, engine);
                        if self.settings.voice_sounds && !self.deafened {
                            self.settings.play(crate::sounds::Sound::Join);
                        }
                        if self.muted || self.deafened {
                            self.apply_mute_state();
                        }
                    }
                    Err(e) => {
                        self.notice = Some(format!("Couldn't start voice: {e}"));
                        self.send(ClientMsg::LeaveVoice);
                    }
                }
            }
            ServerMsg::VoiceStateUpdate { user_id, state } => {
                let Some(s) = &mut self.session else { return };
                let me = s.me;
                // Someone else came into or left the voice channel we're in.
                if let Some(mine) = self.voice.as_ref().map(|v| v.channel_id)
                    && user_id != me
                    && self.settings.voice_sounds
                    && !self.deafened
                {
                    let was_here = s.voice.get(&user_id).is_some_and(|v| v.channel_id == mine);
                    let is_here = state.as_ref().is_some_and(|v| v.channel_id == mine);
                    match (was_here, is_here) {
                        (false, true) => self.settings.play(crate::sounds::Sound::Join),
                        (true, false) => self.settings.play(crate::sounds::Sound::Leave),
                        _ => {}
                    }
                }
                match &state {
                    Some(st) => s.voice.insert(user_id, st.clone()),
                    None => s.voice.remove(&user_id),
                };
                if user_id == me {
                    let my_ssrc = self.voice.as_ref().map(|v| v.ssrc);
                    match (state, my_ssrc) {
                        // Removed by the server (e.g. the channel was deleted).
                        (None, Some(_)) => self.stop_voice(),
                        (Some(st), Some(ssrc)) if st.ssrc != ssrc && self.voice_joining.is_none() => {
                            self.stop_voice();
                            self.notice = Some("You joined voice from another device.".into());
                        }
                        _ => {}
                    }
                } else if let (Some(st), Some(v)) = (&state, &self.voice) {
                    v.engine.set_speaker_volume(st.ssrc, self.settings.user_volume(user_id));
                }
            }
            _ => {}
        }
    }

    /// Drop back to the login screen.
    fn end_session(&mut self, error: Option<String>) {
        self.stop_voice();
        self.pending.clear();
        self.emoji_picker = None;
        self.images = ImageCache::default();
        self.mic_test = None;
        self.voice_joining = None;
        self.rejoin_voice = None;
        self.generation += 1;
        self.conn = None;
        self.session = None;
        self.settings.token = None;
        self.settings.save();
        self.settings_tab = None;
        self.overlay = None;
        self.editing = None;
        self.reconnecting = false;
        self.login.busy = false;
        self.login.password.clear();
        self.login.error = error;
    }

    pub(crate) fn select_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        if s.current != Some(id) {
            self.composer.clear();
            self.editing = None;
            self.message_menu = None;
            self.selecting = None;
            self.last_typing_sent = None;
            self.emoji_picker = None;
            self.pending.clear();
        }
        s.current = Some(id);
        if s.channels.iter().any(|c| c.id == id) {
            s.view = View::Server;
            s.last_server_channel = Some(id);
        } else {
            s.view = View::Home;
        }
        self.pending_channel = Some(id);
        let st = s.chans.entry(id).or_default();
        if !st.loaded && !st.loading {
            st.loading = true;
            if let Some(conn) = &self.conn {
                conn.send(ClientMsg::FetchHistory { channel_id: id, before: None });
            }
        }
        self.mark_current_read();
        Task::batch([Self::snap_to_end(), operation::focus(Id::new(COMPOSER_ID)), self.request_pictures()])
    }

    fn on_net(&mut self, event: net::Event) -> Task<Message> {
        match event {
            net::Event::Connected(conn) => {
                if let Some(auth) = self.pending_auth.take() {
                    conn.send(auth);
                }
                self.conn = Some(conn);
            }
            net::Event::Disconnected(reason) => {
                self.conn = None;
                if let Some(channel) = self.voice.as_ref().map(|v| v.channel_id).or(self.voice_joining.take()) {
                    self.rejoin_voice = Some(channel);
                }
                self.stop_voice();
                if self.session.is_some() {
                    // Keep the UI and retry with growing delays (1s, 2s, 4s ... max 30s).
                    self.reconnecting = true;
                    let delay = Duration::from_secs((1u64 << self.reconnect_attempts.min(5)).min(30));
                    self.reconnect_attempts += 1;
                    let generation = self.generation;
                    return Task::perform(tokio::time::sleep(delay), move |_| Message::Reconnect(generation));
                }
                self.login.busy = false;
                self.login.error = Some(reason);
            }
            net::Event::Received(msg) => return self.on_server(*msg),
        }
        Task::none()
    }

    fn on_ready(&mut self, ready: Ready) -> Task<Message> {
        self.settings.token = Some(ready.token);
        self.settings.save();
        self.login.busy = false;
        self.login.password.clear();
        self.login.invite.clear();
        self.login.error = None;
        self.reconnecting = false;
        self.reconnect_attempts = 0;

        // On a reconnect, stay in the same channel but refetch everything.
        let previous = self.session.as_ref().and_then(|s| s.current);
        let current = previous
            .filter(|id| ready.channels.iter().any(|c| c.id == *id) || ready.directs.iter().any(|d| d.id == *id))
            .or_else(|| ready.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id));
        let mut latest: HashMap<ChannelId, MessageId> = HashMap::new();
        for c in &ready.channels {
            if let Some(m) = c.last_message_id {
                latest.insert(c.id, m);
            }
        }
        for d in &ready.directs {
            if let Some(m) = d.last_message_id {
                latest.insert(d.id, m);
            }
        }

        self.session = Some(Session {
            me: ready.me,
            community: ready.community_name,
            users: ready.users.into_iter().map(|u| (u.id, u)).collect(),
            online: ready.online.into_iter().collect(),
            channels: ready.channels,
            // Keeping the previous channel means select_channel won't wipe a half-typed message.
            current: previous,
            chans: HashMap::new(),
            typing: HashMap::new(),
            voice: ready.voice.into_iter().map(|v| (v.user_id, v)).collect(),
            view: View::Server,
            directs: ready.directs,
            read: ready.read_states.into_iter().map(|r| (r.channel_id, r.last_read)).collect(),
            latest,
            last_server_channel: None,
            server_version: ready.server_version,
            gif_provider: ready.gif_provider,
            max_upload_mb: ready.max_upload_mb,
        });
        self.gifs = GifSearch::default();
        if let Some(channel_id) = self.rejoin_voice.take()
            && self.session.as_ref().is_some_and(|s| s.channels.iter().any(|c| c.id == channel_id))
        {
            self.voice_joining = Some(channel_id);
            self.send(ClientMsg::JoinVoice { channel_id });
        }
        match current {
            Some(id) => self.select_channel(id),
            None => Task::none(),
        }
    }

    fn on_server(&mut self, msg: ServerMsg) -> Task<Message> {
        if let ServerMsg::Ready(ready) = msg {
            return self.on_ready(ready);
        }
        if let ServerMsg::AuthFailed { reason } = msg {
            self.end_session(Some(reason));
            return Task::none();
        }
        if matches!(msg, ServerMsg::VoiceReady { .. } | ServerMsg::VoiceStateUpdate { .. }) {
            self.on_voice(msg);
            return Task::none();
        }
        if matches!(msg, ServerMsg::DirectOpened(_) | ServerMsg::ReactionUpdate { .. } | ServerMsg::GifResults { .. }) {
            return self.on_social(msg);
        }
        let me = self.session.as_ref().map(|s| s.me);
        let Some(s) = &mut self.session else { return Task::none() };

        match msg {
            ServerMsg::Ready(_) | ServerMsg::AuthFailed { .. } | ServerMsg::Pong => {}
            ServerMsg::VoiceReady { .. } | ServerMsg::VoiceStateUpdate { .. } => {} // handled in on_voice
            ServerMsg::DirectOpened(_) | ServerMsg::ReactionUpdate { .. } | ServerMsg::GifResults { .. } => {} // on_social
            ServerMsg::MessageCreated(m) => {
                if let Some(t) = s.typing.get_mut(&m.channel_id) {
                    t.remove(&m.author_id);
                }
                let channel = m.channel_id;
                // Chime for messages from others (not while deafened).
                if Some(m.author_id) != me && !self.deafened {
                    let in_server = s.channels.iter().any(|c| c.id == channel);
                    if (in_server && self.settings.server_sound) || (!in_server && self.settings.dm_sound) {
                        self.settings.play(crate::sounds::Sound::Message);
                    }
                }
                let latest = s.latest.entry(channel).or_insert(m.id);
                *latest = (*latest).max(m.id);
                if Some(m.author_id) == me {
                    s.read.insert(channel, m.id);
                }
                // Most recent conversation first.
                if let Some(pos) = s.directs.iter().position(|d| d.id == channel) {
                    let mut d = s.directs.remove(pos);
                    d.last_message_id = Some(m.id);
                    s.directs.insert(0, d);
                }
                let st = s.chans.entry(channel).or_default();
                if st.loaded && !st.messages.iter().any(|x| x.id == m.id) {
                    st.messages.push(m);
                }
                if s.current == Some(channel) {
                    self.mark_current_read();
                    return self.request_pictures();
                }
            }
            ServerMsg::MessageUpdated(m) => {
                if let Some(st) = s.chans.get_mut(&m.channel_id)
                    && let Some(slot) = st.messages.iter_mut().find(|x| x.id == m.id)
                {
                    *slot = m;
                }
            }
            ServerMsg::MessageDeleted { channel_id, message_id } => {
                if let Some(st) = s.chans.get_mut(&channel_id) {
                    st.messages.retain(|m| m.id != message_id);
                }
                if self.editing.as_ref().is_some_and(|(id, _)| *id == message_id) {
                    self.editing = None;
                }
            }
            ServerMsg::History { channel_id, messages, reached_start } => {
                let st = s.chans.entry(channel_id).or_default();
                let initial = !st.loaded;
                if initial {
                    st.messages = messages;
                } else {
                    let mut older = messages;
                    older.append(&mut st.messages);
                    st.messages = older;
                }
                st.loaded = true;
                st.loading = false;
                st.reached_start = reached_start;
                if s.current == Some(channel_id) {
                    self.mark_current_read();
                    let pictures = self.request_pictures();
                    return if initial { Task::batch([Self::snap_to_end(), pictures]) } else { pictures };
                }
            }
            ServerMsg::Typing { channel_id, user_id } => {
                if Some(user_id) != me {
                    s.typing.entry(channel_id).or_default().insert(user_id, Instant::now());
                }
            }
            ServerMsg::UserJoined(u) | ServerMsg::UserUpdated(u) => {
                s.users.insert(u.id, u);
            }
            ServerMsg::Presence { user_id, online } => {
                if online {
                    s.online.insert(user_id);
                } else {
                    s.online.remove(&user_id);
                    for t in s.typing.values_mut() {
                        t.remove(&user_id);
                    }
                }
            }
            ServerMsg::ChannelCreated(c) => {
                if !s.channels.iter().any(|x| x.id == c.id) {
                    s.channels.push(c);
                    s.channels.sort_by_key(|c| (c.position, c.id));
                }
            }
            ServerMsg::ChannelDeleted { channel_id } => {
                s.channels.retain(|c| c.id != channel_id);
                s.chans.remove(&channel_id);
                if s.current == Some(channel_id) {
                    s.current = None;
                    if let Some(first) = s.first_text_channel() {
                        return self.select_channel(first);
                    }
                }
            }
            ServerMsg::Error { message } => {
                // A failed GIF search shows its error in the picker.
                if self.gifs.waiting.take().is_some() && message.contains("GIF search") {
                    self.gifs.error = Some(message);
                } else {
                    self.notice = Some(friendly_error(message));
                }
            }
        }
        Task::none()
    }
}

/// Old servers reject requests they don't know with a technical message
/// ("unknown variant `OpenDirect`"); explain what's actually going on.
fn friendly_error(message: String) -> String {
    if message.starts_with("bad request: unknown variant") {
        "The server is running an older version of SynCord that doesn't have this feature yet. \
         Whoever runs the server needs to install the latest SynCord Server."
            .into()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::friendly_error;

    #[test]
    fn explains_outdated_servers() {
        let raw = "bad request: unknown variant `OpenDirect`, expected one of `Register`, `Login`".to_string();
        assert!(friendly_error(raw).contains("older version"));
        assert_eq!(friendly_error("Wrong password".into()), "Wrong password");
    }
}
