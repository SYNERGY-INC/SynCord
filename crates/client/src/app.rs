//! Application state and update logic.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use iced::widget::{Id, operation};
use iced::{Subscription, Task, keyboard};
use syncord_proto::{
    AvatarRing, Channel, ChannelId, ClientMsg, Message as ChatMessage, MessageId, Profile, Ready, ServerMsg, User,
    UserId,
};

use crate::net::{self, Connection};
use crate::settings::Settings;
use crate::theme::{self, AppTheme, Hex, to_u32};

pub const MESSAGES_ID: &str = "messages";
pub const COMPOSER_ID: &str = "composer";
const TYPING_SHOWN_FOR: Duration = Duration::from_secs(6);
const TYPING_RESEND_AFTER: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
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
    ToggleMembers,
    DismissNotice,

    // Overlays
    ShowProfile(UserId),
    OpenNewChannel,
    NewChannelName(String),
    NewChannelTopic(String),
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
    TextScale(f32),
    ToggleCompact(bool),
    Logout,

    Tick,
    Escape,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Profile(UserId),
    NewChannel { name: String, topic: String },
    ConfirmDelete(ChannelId),
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
    pub unread: HashSet<ChannelId>,
    pub typing: HashMap<ChannelId, HashMap<UserId, Instant>>,
}

impl Session {
    pub fn me(&self) -> Option<&User> {
        self.users.get(&self.me)
    }

    pub fn current_channel(&self) -> Option<&Channel> {
        self.current.and_then(|id| self.channels.iter().find(|c| c.id == id))
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

pub struct App {
    pub settings: Settings,
    pub themes: Vec<AppTheme>,
    pub theme_errors: Vec<String>,
    pub theme: AppTheme,

    generation: u64,
    conn: Option<Connection>,
    pending_auth: Option<ClientMsg>,
    reconnect_attempts: u32,
    pub reconnecting: bool,

    pub login: LoginForm,
    pub session: Option<Session>,

    pub composer: String,
    last_typing_sent: Option<Instant>,
    pub hovered: Option<MessageId>,
    pub editing: Option<(MessageId, String)>,
    pub overlay: Option<Overlay>,
    pub settings_tab: Option<SettingsTab>,
    pub draft: ProfileDraft,
    pub notice: Option<String>,
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
            generation: 0,
            conn: None,
            pending_auth: None,
            reconnect_attempts: 0,
            reconnecting: false,
            session: None,
            composer: String::new(),
            last_typing_sent: None,
            hovered: None,
            editing: None,
            overlay: None,
            settings_tab: None,
            draft: ProfileDraft::default(),
            notice: None,
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
        (app, task)
    }

    pub fn title(&self) -> String {
        match self.session.as_ref().and_then(|s| s.current_channel()) {
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
        let keys = keyboard::listen().filter_map(|event| match event {
            keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. } => {
                Some(Message::Escape)
            }
            _ => None,
        });
        let someone_typing = self.session.as_ref().is_some_and(|s| s.typing.values().any(|m| !m.is_empty()));
        if someone_typing {
            Subscription::batch([keys, iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick)])
        } else {
            keys
        }
    }

    fn connect(&mut self, auth: ClientMsg) -> Task<Message> {
        self.generation += 1;
        self.conn = None;
        self.pending_auth = Some(auth);
        let generation = self.generation;
        let url = net::server_url(&self.settings.server);
        Task::run(net::connect(url), move |event| Message::Net(generation, event))
    }

    fn send(&self, msg: ClientMsg) {
        if let Some(conn) = &self.conn {
            conn.send(msg);
        }
    }

    fn snap_to_end() -> Task<Message> {
        operation::snap_to_end(Id::new(MESSAGES_ID))
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
                if typing_now && self.last_typing_sent.is_none_or(|t| t.elapsed() > TYPING_RESEND_AFTER) {
                    if let Some(channel_id) = self.session.as_ref().and_then(|s| s.current) {
                        self.send(ClientMsg::Typing { channel_id });
                        self.last_typing_sent = Some(Instant::now());
                    }
                }
            }
            Message::SendMessage => {
                let content = self.composer.trim().to_string();
                if content.is_empty() {
                    return Task::none();
                }
                if let Some(channel_id) = self.session.as_ref().and_then(|s| s.current) {
                    if self.conn.is_none() {
                        self.notice = Some("Not connected right now; your message wasn't sent.".into());
                        return Task::none();
                    }
                    self.send(ClientMsg::SendMessage { channel_id, content });
                    self.composer.clear();
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
            Message::OpenNewChannel => {
                self.overlay = Some(Overlay::NewChannel { name: String::new(), topic: String::new() });
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
            Message::CreateChannel => {
                if let Some(Overlay::NewChannel { name, topic }) = self.overlay.take() {
                    self.send(ClientMsg::CreateChannel { name, topic });
                }
            }
            Message::AskDeleteChannel(id) => self.overlay = Some(Overlay::ConfirmDelete(id)),
            Message::ConfirmDeleteChannel(channel_id) => {
                self.overlay = None;
                self.send(ClientMsg::DeleteChannel { channel_id });
            }
            Message::CloseOverlay => self.overlay = None,

            Message::OpenSettings => {
                if let Some(me) = self.session.as_ref().and_then(|s| s.me()) {
                    self.draft = ProfileDraft::from_profile(&me.profile);
                }
                self.settings_tab = Some(SettingsTab::Profile);
                self.overlay = None;
            }
            Message::CloseSettings => self.settings_tab = None,
            Message::SettingsTab(tab) => self.settings_tab = Some(tab),
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
                if let Some(t) = self.themes.iter().find(|t| t.name == name) {
                    self.theme = t.clone();
                    self.settings.theme = name;
                    self.settings.save();
                }
            }
            Message::ReloadThemes => {
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
            Message::Logout => {
                self.send(ClientMsg::Logout);
                self.end_session(None);
            }

            Message::Tick => {
                if let Some(s) = &mut self.session {
                    for users in s.typing.values_mut() {
                        users.retain(|_, at| at.elapsed() < TYPING_SHOWN_FOR);
                    }
                }
            }
            Message::Escape => {
                if self.overlay.is_some() {
                    self.overlay = None;
                } else if self.editing.is_some() {
                    self.editing = None;
                } else if self.settings_tab.is_some() {
                    self.settings_tab = None;
                }
            }
        }
        Task::none()
    }

    /// Drop back to the login screen.
    fn end_session(&mut self, error: Option<String>) {
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

    fn select_channel(&mut self, id: ChannelId) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        if s.current != Some(id) {
            self.composer.clear();
            self.editing = None;
            self.last_typing_sent = None;
        }
        s.current = Some(id);
        s.unread.remove(&id);
        let st = s.chans.entry(id).or_default();
        if !st.loaded && !st.loading {
            st.loading = true;
            if let Some(conn) = &self.conn {
                conn.send(ClientMsg::FetchHistory { channel_id: id, before: None });
            }
        }
        Task::batch([Self::snap_to_end(), operation::focus(Id::new(COMPOSER_ID))])
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
            .filter(|id| ready.channels.iter().any(|c| c.id == *id))
            .or_else(|| ready.channels.first().map(|c| c.id));

        self.session = Some(Session {
            me: ready.me,
            community: ready.community_name,
            users: ready.users.into_iter().map(|u| (u.id, u)).collect(),
            online: ready.online.into_iter().collect(),
            channels: ready.channels,
            // Keeping the previous channel means select_channel won't wipe a half-typed message.
            current: previous,
            chans: HashMap::new(),
            unread: HashSet::new(),
            typing: HashMap::new(),
        });
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
        let me = self.session.as_ref().map(|s| s.me);
        let Some(s) = &mut self.session else { return Task::none() };

        match msg {
            ServerMsg::Ready(_) | ServerMsg::AuthFailed { .. } | ServerMsg::Pong => {}
            ServerMsg::MessageCreated(m) => {
                if let Some(t) = s.typing.get_mut(&m.channel_id) {
                    t.remove(&m.author_id);
                }
                if s.current != Some(m.channel_id) && Some(m.author_id) != me {
                    s.unread.insert(m.channel_id);
                }
                let st = s.chans.entry(m.channel_id).or_default();
                if st.loaded && !st.messages.iter().any(|x| x.id == m.id) {
                    st.messages.push(m);
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
                if initial && s.current == Some(channel_id) {
                    return Self::snap_to_end();
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
                s.unread.remove(&channel_id);
                if s.current == Some(channel_id) {
                    s.current = None;
                    if let Some(first) = s.channels.first().map(|c| c.id) {
                        return self.select_channel(first);
                    }
                }
            }
            ServerMsg::Error { message } => self.notice = Some(message),
        }
        Task::none()
    }
}
