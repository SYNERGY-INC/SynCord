//! Threads (0.7): side conversations started from a message. A thread opens in the side
//! panel with its own messages and message box; its id works as a channel id for history,
//! sending, typing and read markers.

use std::time::{Duration, Instant};

use celerity_proto::{ChannelId, ChannelKind, ClientMsg, Message as ChatMessage, MessageId, ServerMsg, Thread, perms};
use iced::Task;
use iced::widget::{Id, operation};

use crate::app::{App, Message, Session};
use crate::features::ConfirmAction;

pub const THREAD_MESSAGES_ID: &str = "thread-messages";
pub const THREAD_COMPOSER_ID: &str = "thread-composer";
const TYPING_RESEND_AFTER: Duration = Duration::from_secs(4);

/// The thread panel's message box and what's being done in it (one thread at a time).
#[derive(Default)]
pub struct ThreadUi {
    pub draft: String,
    /// Message in the thread the next one replies to.
    pub replying: Option<MessageId>,
    /// The new name, while renaming.
    pub renaming: Option<String>,
    /// A thread was asked for from this message: open it when it's made.
    pub(crate) pending_from: Option<MessageId>,
    last_typing: Option<Instant>,
}

/// The list of a channel's threads (archived ones too, once the server answers).
pub struct ThreadList {
    pub channel: ChannelId,
    pub threads: Option<Vec<Thread>>,
}

#[derive(Debug, Clone)]
pub enum ThreadMsg {
    /// Start a thread from a message (or open the one it has).
    Create(MessageId),
    Open(ChannelId),
    Close,
    /// The threads button in the channel header.
    ToggleList,
    Draft(String),
    Send,
    LoadOlder,
    StartRename,
    RenameInput(String),
    SaveRename,
    CancelRename,
    Archive(ChannelId, bool),
    AskDelete(ChannelId),
    CancelReply,
}

impl Session {
    /// A text channel of this server where threads can be started.
    pub fn threads_allowed(&self, channel: ChannelId) -> bool {
        self.v07 && self.channels.iter().any(|c| c.id == channel && c.kind == ChannelKind::Text)
    }

    /// Whether you may rename and archive a thread.
    pub fn can_edit_thread(&self, t: &Thread) -> bool {
        t.owner_id == self.me || self.can(perms::MANAGE_MESSAGES)
    }

    pub fn can_delete_thread(&self) -> bool {
        self.can(perms::MANAGE_MESSAGES) || self.can(perms::MANAGE_CHANNELS)
    }

    /// The thread open in the side panel.
    pub fn open_thread(&self) -> Option<&Thread> {
        self.threads.get(&self.open_thread?)
    }
}

impl App {
    pub(crate) fn update_threads(&mut self, msg: ThreadMsg) -> Task<Message> {
        match msg {
            ThreadMsg::Create(message_id) => {
                let Some(s) = &self.session else { return Task::none() };
                let Some(m) = self.find_message(message_id) else { return Task::none() };
                let (channel_id, existing) = (m.channel_id, m.thread.as_ref().map(|t| t.id));
                if let Some(id) = existing {
                    return self.update_threads(ThreadMsg::Open(id));
                }
                if !s.threads_allowed(channel_id) {
                    return Task::none();
                }
                self.message_menu = None;
                self.thread_ui.pending_from = Some(message_id);
                self.send(ClientMsg::CreateThread { channel_id, message_id: Some(message_id), name: String::new() });
            }
            ThreadMsg::Open(id) => return self.open_thread(id),
            ThreadMsg::Close => {
                if let Some(s) = &mut self.session {
                    s.open_thread = None;
                }
                self.thread_ui = ThreadUi::default();
            }
            ThreadMsg::ToggleList => {
                let Some(s) = &mut self.session else { return Task::none() };
                let Some(channel) = s.current else { return Task::none() };
                if s.thread_list.as_ref().is_some_and(|l| l.channel == channel) {
                    s.thread_list = None;
                } else {
                    s.thread_list = Some(ThreadList { channel, threads: None });
                    s.open_thread = None;
                    self.search.open = false;
                    self.pins = None;
                    self.send(ClientMsg::ListThreads { channel_id: channel });
                }
            }
            ThreadMsg::Draft(text) => {
                self.thread_ui.draft = text;
                let thread = self.session.as_ref().and_then(|s| s.open_thread);
                if let Some(channel_id) = thread
                    && !self.thread_ui.draft.trim().is_empty()
                    && self.thread_ui.last_typing.is_none_or(|t| t.elapsed() > TYPING_RESEND_AFTER)
                {
                    self.thread_ui.last_typing = Some(Instant::now());
                    self.send(ClientMsg::Typing { channel_id });
                }
            }
            ThreadMsg::Send => {
                let Some(channel_id) = self.session.as_ref().and_then(|s| s.open_thread) else { return Task::none() };
                let content = self.expand_custom_emoji(self.thread_ui.draft.trim());
                let content = self.encode_mentions(&content, &[]);
                if content.is_empty() {
                    return Task::none();
                }
                if self.conn.is_none() {
                    self.notice = Some("Not connected right now; your message wasn't sent.".into());
                    return Task::none();
                }
                let reply_to = self.thread_ui.replying.take();
                self.send(ClientMsg::SendMessage { channel_id, content, attachments: Vec::new(), reply_to });
                self.thread_ui.draft.clear();
                self.thread_ui.last_typing = None;
                return operation::snap_to(Id::new(THREAD_MESSAGES_ID), operation::RelativeOffset::START);
            }
            ThreadMsg::LoadOlder => {
                if let Some(s) = &mut self.session
                    && let Some(id) = s.open_thread
                {
                    let st = s.chans.entry(id).or_default();
                    if !st.loading && !st.reached_start {
                        st.loading = true;
                        let before = st.messages.first().map(|m| m.id);
                        self.send(ClientMsg::FetchHistory { channel_id: id, before });
                    }
                }
            }
            ThreadMsg::StartRename => {
                self.thread_ui.renaming = self.session.as_ref().and_then(|s| s.open_thread()).map(|t| t.name.clone());
                return operation::focus(Id::new("thread-rename"));
            }
            ThreadMsg::RenameInput(v) => {
                if let Some(name) = &mut self.thread_ui.renaming {
                    *name = v;
                }
            }
            ThreadMsg::SaveRename => {
                let name = self.thread_ui.renaming.take().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
                if let (Some(name), Some(thread_id)) = (name, self.session.as_ref().and_then(|s| s.open_thread)) {
                    self.send(ClientMsg::UpdateThread { thread_id, name: Some(name), archived: None });
                }
            }
            ThreadMsg::CancelRename => self.thread_ui.renaming = None,
            ThreadMsg::Archive(thread_id, archived) => {
                self.send(ClientMsg::UpdateThread { thread_id, name: None, archived: Some(archived) });
            }
            ThreadMsg::AskDelete(id) => {
                let name = self.session.as_ref().and_then(|s| s.threads.get(&id)).map(|t| t.name.clone());
                if let Some(name) = name {
                    self.ask(
                        "Delete thread",
                        format!("Delete \"{name}\" and all of its messages? This can't be undone."),
                        "Delete Thread",
                        ConfirmAction::DeleteThread(id),
                    );
                }
            }
            ThreadMsg::CancelReply => self.thread_ui.replying = None,
        }
        Task::none()
    }

    fn open_thread(&mut self, id: ChannelId) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        if !s.threads.contains_key(&id) {
            return Task::none();
        }
        if s.open_thread != Some(id) {
            self.thread_ui = ThreadUi::default();
        }
        s.open_thread = Some(id);
        s.thread_list = None;
        self.search.open = false;
        self.pins = None;
        self.message_menu = None;
        let st = s.chans.entry(id).or_default();
        if (!st.loaded || st.stale) && !st.loading {
            st.loading = true;
            self.send(ClientMsg::FetchHistory { channel_id: id, before: None });
        }
        self.mark_thread_read();
        Task::batch([
            operation::snap_to(Id::new(THREAD_MESSAGES_ID), operation::RelativeOffset::START),
            operation::focus(Id::new(THREAD_COMPOSER_ID)),
        ])
    }

    /// Tell the server the open thread has been read up to its newest message.
    fn mark_thread_read(&mut self) {
        let Some(s) = &mut self.session else { return };
        let Some(id) = s.open_thread else { return };
        let newest = s.chans.get(&id).and_then(|st| st.messages.last()).map(|m| m.id);
        let latest = s.threads.get(&id).and_then(|t| t.last_message_id).max(newest);
        let Some(latest) = latest.filter(|l| s.read.get(&id).is_none_or(|r| r < l)) else { return };
        s.read.insert(id, latest);
        self.send(ClientMsg::MarkRead { channel_id: id, message_id: latest });
    }

    /// A message arrived: if it's in the thread you're looking at, it's read.
    pub(crate) fn on_thread_message(&mut self, m: &ChatMessage) {
        if !self.background && self.session.as_ref().is_some_and(|s| s.open_thread == Some(m.channel_id)) {
            self.mark_thread_read();
        }
    }

    /// Another channel was opened: its threads, not this one's, belong beside it.
    pub(crate) fn close_other_threads(&mut self, channel: ChannelId) {
        let Some(s) = &mut self.session else { return };
        if s.open_thread().is_some_and(|t| t.parent_id != channel) {
            s.open_thread = None;
            self.thread_ui = ThreadUi::default();
        }
        if s.thread_list.as_ref().is_some_and(|l| l.channel != channel) {
            s.thread_list = None;
        }
    }

    /// Escape: close the thread (or thread list) panel. True if it did.
    pub(crate) fn close_thread_panel(&mut self) -> bool {
        let Some(s) = &mut self.session else { return false };
        let open = s.open_thread.is_some() || s.thread_list.is_some();
        s.open_thread = None;
        s.thread_list = None;
        open
    }

    pub(crate) fn on_thread_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let background = self.background;
        let Some(s) = &mut self.session else { return Task::none() };
        match msg {
            ServerMsg::ThreadCreated(t) | ServerMsg::ThreadUpdated(t) => {
                // The message it started from shows its latest name and count.
                if let Some(m) = s
                    .chans
                    .get_mut(&t.parent_id)
                    .and_then(|st| st.messages.iter_mut().find(|m| m.thread.as_ref().is_some_and(|x| x.id == t.id)))
                {
                    m.thread = Some(t.summary());
                }
                if let Some(list) = s.thread_list.as_mut().filter(|l| l.channel == t.parent_id) {
                    let threads = list.threads.get_or_insert_with(Vec::new);
                    match threads.iter_mut().find(|x| x.id == t.id) {
                        Some(slot) => *slot = t.clone(),
                        None => threads.insert(0, t.clone()),
                    }
                }
                if let Some(m) = t.last_message_id {
                    s.latest.insert(t.id, m);
                }
                let (id, from) = (t.id, t.parent_message_id);
                s.threads.insert(t.id, t);
                if !background && from.is_some() && self.thread_ui.pending_from == from {
                    self.thread_ui.pending_from = None;
                    return self.open_thread(id);
                }
            }
            ServerMsg::ThreadDeleted { thread_id, parent_id } => {
                s.threads.remove(&thread_id);
                s.chans.remove(&thread_id);
                if let Some(list) = s.thread_list.as_mut().filter(|l| l.channel == parent_id)
                    && let Some(threads) = &mut list.threads
                {
                    threads.retain(|t| t.id != thread_id);
                }
                if s.open_thread == Some(thread_id) {
                    s.open_thread = None;
                    self.thread_ui = ThreadUi::default();
                }
            }
            ServerMsg::Threads { channel_id, mut threads } => {
                threads.sort_by_key(|t| std::cmp::Reverse(t.last_activity));
                for t in &threads {
                    s.threads.insert(t.id, t.clone());
                }
                if let Some(list) = s.thread_list.as_mut().filter(|l| l.channel == channel_id) {
                    list.threads = Some(threads);
                }
            }
            _ => {}
        }
        Task::none()
    }
}
