//! Several servers at once.
//!
//! The open server lives in the app's own fields (`session`, `conn`, `settings.server`...),
//! so everything else in the app only ever deals with one server. The others are parked
//! here with their own connection. A message from a parked server is handled by swapping
//! it in for a moment with `background` set, so it updates its own unreads and pings
//! without touching what's on screen.
//!
//! A voice call belongs to the server it was started on (`call_uid`); call controls go
//! there even while another server is open.

use std::sync::atomic::{AtomicU64, Ordering};

use celerity_proto::ClientMsg;
use iced::Task;

use crate::app::{App, LoginForm, Message, Session};
use crate::net::{self, Connection};
use crate::settings::SavedServer;

/// Connection generations are unique across servers, so a message says which one it's from.
pub(crate) fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Identifies a server for as long as Celerity runs (connections come and go).
pub(crate) fn next_uid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A server that isn't open.
pub struct Parked {
    pub saved: SavedServer,
    pub uid: u64,
    generation: u64,
    conn: Option<Connection>,
    pending_auth: Option<ClientMsg>,
    reconnect_attempts: u32,
    reconnecting: bool,
    pub login: LoginForm,
    pub session: Option<Session>,
    http: reqwest::Client,
    server_url: Option<String>,
    encrypted: Option<Option<String>>,
    identity_changed: Option<String>,
    sent_presence: Option<(celerity_proto::Status, Option<celerity_proto::Activity>)>,
    rejoin_voice: Option<celerity_proto::ChannelId>,
}

impl Parked {
    fn new(saved: SavedServer) -> Self {
        Parked {
            login: LoginForm { server: saved.server.clone(), username: saved.username.clone(), ..Default::default() },
            saved,
            uid: next_uid(),
            generation: 0,
            conn: None,
            pending_auth: None,
            reconnect_attempts: 0,
            reconnecting: false,
            session: None,
            http: crate::media::client(),
            server_url: None,
            encrypted: None,
            identity_changed: None,
            sent_presence: None,
            rejoin_voice: None,
        }
    }

    pub fn name(&self) -> String {
        match &self.session {
            Some(s) => s.community.clone(),
            None => self.saved.server.clone(),
        }
    }
}

/// What the server rail shows for one server.
pub struct RailEntry<'a> {
    pub position: usize,
    /// The server's address as settings key it (`net::server_key`).
    pub key: String,
    pub open: bool,
    pub name: String,
    pub session: Option<&'a Session>,
    /// Signed out, or can't connect.
    pub offline: bool,
}

impl App {
    /// Exchange the open server with parked server `i` (both ways: calling it twice undoes it).
    fn swap_parked(&mut self, i: usize) {
        let p = &mut self.parked[i];
        std::mem::swap(&mut self.settings.server, &mut p.saved.server);
        std::mem::swap(&mut self.settings.username, &mut p.saved.username);
        std::mem::swap(&mut self.settings.token, &mut p.saved.token);
        std::mem::swap(&mut self.server_uid, &mut p.uid);
        std::mem::swap(&mut self.generation, &mut p.generation);
        std::mem::swap(&mut self.conn, &mut p.conn);
        std::mem::swap(&mut self.pending_auth, &mut p.pending_auth);
        std::mem::swap(&mut self.reconnect_attempts, &mut p.reconnect_attempts);
        std::mem::swap(&mut self.reconnecting, &mut p.reconnecting);
        std::mem::swap(&mut self.login, &mut p.login);
        std::mem::swap(&mut self.session, &mut p.session);
        std::mem::swap(&mut self.http, &mut p.http);
        std::mem::swap(&mut self.server_url, &mut p.server_url);
        std::mem::swap(&mut self.encrypted, &mut p.encrypted);
        std::mem::swap(&mut self.identity_changed, &mut p.identity_changed);
        std::mem::swap(&mut self.sent_presence, &mut p.sent_presence);
        std::mem::swap(&mut self.rejoin_voice, &mut p.rejoin_voice);
        self.mirror_parked();
    }

    /// Keep the saved list in step with the parked servers.
    fn mirror_parked(&mut self) {
        self.settings.other_servers = self.parked.iter().map(|p| p.saved.clone()).collect();
    }

    /// Run `f` as if parked server `i` were open, without changing what's on screen.
    fn in_background<T>(&mut self, i: usize, f: impl FnOnce(&mut App) -> T) -> T {
        self.swap_parked(i);
        let was = std::mem::replace(&mut self.background, true);
        let out = f(self);
        self.background = was;
        self.swap_parked(i);
        out
    }

    fn parked_with_generation(&self, generation: u64) -> Option<usize> {
        self.parked.iter().position(|p| p.generation == generation && generation != 0)
    }

    /// A message from a connection that isn't the open server's.
    pub(crate) fn on_parked_net(&mut self, generation: u64, event: net::Event) -> Task<Message> {
        let Some(i) = self.parked_with_generation(generation) else { return Task::none() };
        let task = self.in_background(i, |app| app.on_net(event));
        // Pictures and avatars it asked for are fine to fetch; nothing it does scrolls or focuses
        // (those check `background`).
        self.settings.save();
        task
    }

    pub(crate) fn reconnect_parked(&mut self, generation: u64) -> Task<Message> {
        let Some(i) = self.parked_with_generation(generation) else { return Task::none() };
        self.in_background(i, |app| match app.settings.token.clone() {
            Some(token) if app.session.is_some() => app.connect(ClientMsg::Resume { token }),
            _ => Task::none(),
        })
    }

    /// Sign back in to the servers saved last time.
    pub(crate) fn start_parked(&mut self) -> Task<Message> {
        self.parked = self.settings.other_servers.iter().cloned().map(Parked::new).collect();
        let mut tasks = Vec::new();
        for i in 0..self.parked.len() {
            tasks.push(self.in_background(i, |app| match app.settings.token.clone() {
                Some(token) => {
                    app.login.busy = true;
                    app.connect(ClientMsg::Resume { token })
                }
                None => Task::none(),
            }));
        }
        Task::batch(tasks)
    }

    /// Every server in rail order.
    pub fn rail(&self) -> Vec<RailEntry<'_>> {
        let open_pos = self.settings.server_position.min(self.parked.len());
        let mut out: Vec<RailEntry> = self
            .parked
            .iter()
            .enumerate()
            .map(|(i, p)| RailEntry {
                position: if i < open_pos { i } else { i + 1 },
                key: net::server_key(&p.saved.server),
                open: false,
                name: p.name(),
                session: p.session.as_ref(),
                offline: p.session.is_none() || p.reconnecting,
            })
            .collect();
        out.insert(
            open_pos,
            RailEntry {
                position: open_pos,
                key: net::server_key(&self.settings.server),
                open: true,
                name: self
                    .session
                    .as_ref()
                    .map(|s| s.community.clone())
                    .unwrap_or_else(|| self.settings.server.clone()),
                session: self.session.as_ref(),
                offline: self.session.is_none(),
            },
        );
        out
    }

    /// Open the server at `position` in the rail.
    pub(crate) fn switch_server(&mut self, position: usize) -> Task<Message> {
        let open_pos = self.settings.server_position.min(self.parked.len());
        if position == open_pos || position > self.parked.len() {
            return Task::none();
        }
        let j = if position < open_pos { position } else { position - 1 };
        self.leave_screen_state();
        self.swap_parked(j);
        // Put the server that was open back in its place in the rail.
        let old = self.parked.remove(j);
        self.parked.insert(if open_pos < position { open_pos } else { open_pos - 1 }, old);
        self.settings.server_position = position;
        self.mirror_parked();
        self.settings.save();
        self.reopen_after_switch()
    }

    /// Close what belongs to the open server's screen before showing another one.
    fn leave_screen_state(&mut self) {
        self.overlay = None;
        self.settings_tab = None;
        self.editing = None;
        self.replying = None;
        self.message_menu = None;
        self.emoji_picker = None;
        self.search = Default::default();
        self.pins = None;
        self.new_since = None;
        self.live.stage = false;
        self.role_editing = None;
        self.gifs = Default::default();
        self.composer.clear();
        self.editor = iced::widget::text_editor::Content::new();
    }

    fn reopen_after_switch(&mut self) -> Task<Message> {
        self.on_screen.clear();
        let Some(s) = &mut self.session else { return Task::none() };
        // Open its channel again (fetching it if needed) so the screen fills in.
        let current = s.current.take().or_else(|| s.first_text_channel());
        let avatars = self.request_avatars();
        match current {
            Some(id) => Task::batch([self.select_channel(id), avatars]),
            None => avatars,
        }
    }

    /// The + in the rail: sign in to another server (the open one keeps running).
    pub(crate) fn add_server(&mut self) {
        if self.session.is_none() {
            return; // already on the sign-in screen
        }
        self.leave_screen_state();
        self.add_return = self.settings.server_position.min(self.parked.len());
        let fresh = Parked::new(SavedServer::default());
        self.parked.push(fresh);
        let last = self.parked.len() - 1;
        self.swap_parked(last);
        // The one that was open is now parked at the end; put it back in rail order.
        let old = self.parked.remove(last);
        let open_pos = self.settings.server_position.min(self.parked.len());
        self.parked.insert(open_pos, old);
        self.settings.server_position = self.parked.len();
        self.mirror_parked();
        self.login = LoginForm::default();
        self.adding_server = true;
    }

    /// Give up on adding a server (or signing back in to one): show another one.
    pub(crate) fn cancel_add_server(&mut self) -> Task<Message> {
        if self.parked.is_empty() {
            return Task::none();
        }
        self.adding_server = false;
        self.drop_open_server();
        // Back to the server that was open before.
        let back = self.add_return.min(self.parked.len());
        if back != self.settings.server_position {
            return self.switch_server(back);
        }
        self.reopen_after_switch()
    }

    /// The open server was signed out of: show another signed-in one, if any.
    pub(crate) fn after_sign_out(&mut self) -> Task<Message> {
        if self.parked.is_empty() || self.background {
            return Task::none();
        }
        self.drop_open_server();
        self.login.busy = self.session.is_none() && self.settings.token.is_some();
        self.reopen_after_switch()
    }

    /// Forget the open server and open its neighbour in the rail.
    fn drop_open_server(&mut self) {
        let open_pos = self.settings.server_position.min(self.parked.len());
        let next = open_pos.min(self.parked.len() - 1);
        self.swap_parked(next);
        // `next` now holds the server being dropped.
        let dropped = self.parked.remove(next);
        if self.call_uid == Some(dropped.uid) {
            self.call_uid = None;
        }
        self.settings.server_position = next;
        self.mirror_parked();
        self.settings.save();
    }

    /// Mark every channel on the server at `position` read (DMs stay as they are).
    pub(crate) fn mark_server_read(&mut self, position: usize) -> Task<Message> {
        let open_pos = self.settings.server_position.min(self.parked.len());
        let mark = |app: &mut App| {
            let Some(s) = &mut app.session else { return };
            let mut marks = Vec::new();
            for c in &s.channels {
                if let Some(&latest) = s.latest.get(&c.id)
                    && s.read.get(&c.id).is_none_or(|r| *r < latest)
                {
                    marks.push((c.id, latest));
                }
            }
            for &(channel_id, message_id) in &marks {
                s.read.insert(channel_id, message_id);
                s.mention_counts.remove(&channel_id);
            }
            for (channel_id, message_id) in marks {
                app.send(ClientMsg::MarkRead { channel_id, message_id });
            }
        };
        if position == open_pos {
            mark(self);
        } else if position <= self.parked.len() {
            let j = if position < open_pos { position } else { position - 1 };
            self.in_background(j, mark);
        }
        Task::none()
    }

    // ---- Calls on another server

    /// The call (or the one being joined) is on the open server, or there's none.
    pub fn call_here(&self) -> bool {
        self.call_uid.is_none_or(|u| u == self.server_uid)
    }

    /// The call, if it's on the open server.
    pub fn voice_here(&self) -> Option<&crate::app::ActiveVoice> {
        self.voice.as_ref().filter(|_| self.call_here())
    }

    pub fn is_speaking(&self, user: &celerity_proto::UserId) -> bool {
        self.call_here() && self.speaking.contains(user)
    }

    /// The server the call is on.
    pub fn call_session(&self) -> Option<&Session> {
        if self.call_here() {
            return self.session.as_ref();
        }
        self.parked.iter().find(|p| Some(p.uid) == self.call_uid).and_then(|p| p.session.as_ref())
    }

    /// Where the call is, when it isn't on the open server: its rail position and name.
    pub fn call_elsewhere(&self) -> Option<(usize, String)> {
        if self.call_here() || (self.voice.is_none() && self.voice_joining.is_none()) {
            return None;
        }
        let i = self.parked.iter().position(|p| Some(p.uid) == self.call_uid)?;
        let open_pos = self.settings.server_position.min(self.parked.len());
        Some((if i < open_pos { i } else { i + 1 }, self.parked[i].name()))
    }

    /// Send to the server the call is on.
    pub(crate) fn send_call(&self, msg: ClientMsg) {
        if self.call_here() {
            return self.send(msg);
        }
        if let Some(conn) = self.parked.iter().find(|p| Some(p.uid) == self.call_uid).and_then(|p| p.conn.as_ref()) {
            conn.send(msg);
        }
    }

    /// Run `f` with the call's server open (it's usually the open one already).
    pub(crate) fn on_call_server<T>(&mut self, f: impl FnOnce(&mut App) -> T) -> T {
        match self.parked.iter().position(|p| Some(p.uid) == self.call_uid) {
            Some(i) if !self.call_here() => self.in_background(i, f),
            _ => f(self),
        }
    }

    /// Joining voice on the open server: leave a call elsewhere first.
    pub(crate) fn claim_call(&mut self) {
        if !self.call_here() {
            self.send_call(ClientMsg::LeaveVoice);
        }
        self.call_uid = Some(self.server_uid);
    }
}
