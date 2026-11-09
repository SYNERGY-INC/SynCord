//! WebSocket gateway: authentication, then a loop that handles client
//! requests and forwards broadcast events.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use celerity_proto::voice::token_to_hex;
use celerity_proto::{
    ChannelId, ChannelKind, ClientMsg, DirectChannel, EVERYONE_ROLE, HISTORY_PAGE, MAX_ATTACHMENTS, MAX_MESSAGE_LEN,
    MAX_REACTIONS, MAX_ROLES, Message, MessageId, Ready, Role, RoleId, ServerMsg, Status, UserId,
    clean_voice_channel_name, normalize_channel_name, perms, valid_reaction,
};
use rand::Rng;

mod extra;
mod members;
mod social;
mod sounds;
pub use social::close_due_polls;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::db;
use crate::{AppState, Target};

/// Close idle connections that haven't sent anything (not even a ping) for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Messages-per-window allowed before a client is told to slow down.
const RATE_BURST: u32 = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Voice and video packets queued for a connection that gets them over the WebSocket
/// (when UDP can't get through). High-quality screen shares need a deep queue.
const WS_VOICE_QUEUE: usize = 512;

pub async fn upgrade(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Apps from 0.7 on say which protocol they speak; older ones don't (0).
    let protocol = query.get("protocol").and_then(|v| v.parse().ok()).unwrap_or(0);
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| async move {
        let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let (voice_tx, voice_rx) = mpsc::channel(WS_VOICE_QUEUE);
        Connection {
            state,
            socket,
            user: None,
            token: None,
            rate: RateLimit::new(RATE_BURST),
            signals: RateLimit::new(SIGNAL_BURST),
            conn_id,
            voice_tx,
            protocol,
        }
        .run(voice_rx)
        .await
    })
}

pub fn random_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

fn hash_password(password: &str) -> String {
    let salt = SaltString::encode_b64(&rand::rng().random::<[u8; 16]>()).expect("salt");
    Argon2::default().hash_password(password.as_bytes(), &salt).expect("hash").to_string()
}

pub(crate) fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

fn valid_username(name: &str) -> bool {
    (2..=32).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

struct RateLimit {
    window_start: Instant,
    count: u32,
    burst: u32,
}

impl RateLimit {
    fn new(burst: u32) -> Self {
        Self { window_start: Instant::now(), count: 0, burst }
    }

    fn allow(&mut self) -> bool {
        if self.window_start.elapsed() > RATE_WINDOW {
            self.window_start = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.burst
    }
}

/// Call key-exchange blobs allowed per window (a group call of ten needs a few each).
const SIGNAL_BURST: u32 = 60;

struct Connection {
    state: Arc<AppState>,
    socket: WebSocket,
    user: Option<UserId>,
    token: Option<String>,
    rate: RateLimit,
    /// Separate limit for call key exchanges, which come in bursts.
    signals: RateLimit,
    conn_id: u64,
    /// Voice packets the relay wants delivered over this WebSocket.
    voice_tx: mpsc::Sender<Bytes>,
    /// Protocol version the app speaks ([`celerity_proto::PROTOCOL_VERSION`]; 0 before 0.7).
    protocol: u32,
}

/// Server channels where messages are written: text channels and their threads.
fn is_server_text(kind: ChannelKind) -> bool {
    matches!(kind, ChannelKind::Text | ChannelKind::Thread)
}

/// Direct messages and groups: private conversations, where calls happen too.
fn is_conversation(kind: ChannelKind) -> bool {
    matches!(kind, ChannelKind::Direct | ChannelKind::Group)
}

/// If `me` can see `channel`: its kind and who should get its events.
async fn audience(
    state: &AppState,
    me: UserId,
    channel: ChannelId,
) -> Result<Option<(ChannelKind, Target)>, rusqlite::Error> {
    let Some(kind) = state.db.call(move |c| db::accessible_kind(c, me, channel)).await? else { return Ok(None) };
    Ok(Some((kind, state.channel_target(channel).await?)))
}

/// Whether a voice channel id is a call in a direct message or group.
async fn is_call(db: &db::Db, channel: ChannelId) -> Result<bool, rusqlite::Error> {
    db.call(move |c| Ok(db::channel_kind(c, channel)?.is_some_and(is_conversation))).await
}

/// Who should hear that someone left `old` for `new`: those who could see `old` but won't
/// get the update for `new`.
async fn left_behind(
    state: &AppState,
    old: ChannelId,
    new: Option<ChannelId>,
) -> Result<Option<Target>, rusqlite::Error> {
    let old_t = state.channel_target(old).await?;
    let new_t = match new {
        Some(n) => state.channel_target(n).await?,
        None => return Ok(Some(old_t)),
    };
    if new_t.users.is_none() && !new_t.modern_only {
        return Ok(None); // everyone sees the new state
    }
    let olds: Vec<UserId> = match &old_t.users {
        Some(u) => u.to_vec(),
        None => state.db.call(|c| Ok(db::all_users(c)?.into_iter().map(|u| u.id).collect())).await?,
    };
    let rest: Vec<UserId> = olds.into_iter().filter(|u| !new_t.includes(*u)).collect();
    Ok((!rest.is_empty()).then(|| Target::only(rest).modern(old_t.modern_only)))
}

/// What the main loop should do after handling a frame.
enum Flow {
    Continue,
    Close,
}

impl Connection {
    async fn run(mut self, mut voice_rx: mpsc::Receiver<Bytes>) {
        let mut events = self.state.events.subscribe();

        loop {
            tokio::select! {
                frame = tokio::time::timeout(IDLE_TIMEOUT, self.socket.recv()) => {
                    let frame = match frame {
                        Ok(Some(Ok(frame))) => frame,
                        Ok(Some(Err(e))) => { debug!("socket error: {e}"); break; }
                        Ok(None) => break,
                        Err(_) => { debug!("idle timeout"); break; }
                    };
                    let text = match frame {
                        WsMessage::Text(t) => t,
                        WsMessage::Binary(data) => {
                            // Voice over WebSocket (when UDP can't get through).
                            if self.user.is_some() {
                                self.state.voice.handle_packet(&data, None);
                            }
                            continue;
                        }
                        WsMessage::Close(_) => break,
                        _ => continue,
                    };
                    let msg: ClientMsg = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            self.send(&ServerMsg::Error { message: format!("bad request: {e}") }).await;
                            continue;
                        }
                    };
                    match self.handle(msg).await {
                        Flow::Continue => {}
                        Flow::Close => break,
                    }
                }
                event = events.recv(), if self.user.is_some() => {
                    match event {
                        Ok(event) => {
                            let Some(me) = self.user else { continue };
                            if event.is_for(me, self.protocol)
                                && self.socket.send(WsMessage::Text(event.json.as_ref().into())).await.is_err()
                            {
                                break;
                            }
                            // Kicked or banned: they've been told, now they go.
                            if event.disconnect == Some(me) {
                                self.token = None;
                                break;
                            }
                        }
                        // The client fell too far behind; drop it so it reconnects and resyncs.
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => break,
                    }
                }
                Some(packet) = voice_rx.recv() => {
                    if self.socket.send(WsMessage::Binary(packet)).await.is_err() {
                        break;
                    }
                }
            }
        }

        if let Some(id) = self.user {
            self.leave_voice(id);
            self.set_offline(id);
        }
    }

    async fn send(&mut self, msg: &ServerMsg) -> bool {
        let json = serde_json::to_string(msg).expect("serialize");
        self.socket.send(WsMessage::Text(json.into())).await.is_ok()
    }

    fn broadcast(&self, msg: &ServerMsg) {
        self.broadcast_to(msg, None);
    }

    /// Send to everyone (`None`) or only to the listed people.
    fn broadcast_to(&self, msg: &ServerMsg, to: Option<Arc<[UserId]>>) {
        self.state.broadcast(msg, to);
    }

    fn broadcast_target(&self, msg: &ServerMsg, to: Target) {
        self.state.broadcast_target(msg, to);
    }

    /// Send to everyone who can see `channel` (and whose app knows its kind).
    async fn broadcast_in(&mut self, channel: ChannelId, msg: &ServerMsg) -> Result<(), rusqlite::Error> {
        let to = self.state.channel_target(channel).await?;
        self.broadcast_target(msg, to);
        Ok(())
    }

    async fn error(&mut self, message: impl Into<String>) -> Flow {
        self.send(&ServerMsg::Error { message: message.into() }).await;
        Flow::Continue
    }

    async fn auth_failed(&mut self, reason: impl Into<String>) -> Flow {
        self.send(&ServerMsg::AuthFailed { reason: reason.into() }).await;
        Flow::Continue
    }

    async fn handle(&mut self, msg: ClientMsg) -> Flow {
        // Read markers are cheap and sent automatically, and call key exchanges come in a
        // burst when someone joins, so they don't count towards the limit.
        if !matches!(msg, ClientMsg::MarkRead { .. } | ClientMsg::CallSignal { .. }) && !self.rate.allow() {
            return self.error("You're doing that too fast. Slow down a little.").await;
        }

        let Some(me) = self.user else {
            return match msg {
                ClientMsg::Register { username, password, invite } => self.register(username, password, invite).await,
                ClientMsg::Login { username, password } => self.login(username, password).await,
                ClientMsg::Resume { token } => self.resume(token).await,
                ClientMsg::Ping => {
                    self.send(&ServerMsg::Pong).await;
                    Flow::Continue
                }
                _ => self.auth_failed("Not logged in").await,
            };
        };

        let result: Result<Flow, rusqlite::Error> = async {
            match msg {
                msg @ (ClientMsg::CreateRole { .. }
                | ClientMsg::UpdateRole(_)
                | ClientMsg::DeleteRole { .. }
                | ClientMsg::SetUserRoles { .. }
                | ClientMsg::SetBlocked { .. }
                | ClientMsg::ClearProfileImage { .. }
                | ClientMsg::SetPresence { .. }
                | ClientMsg::Search(_)
                | ClientMsg::SetPinned { .. }
                | ClientMsg::FetchPins { .. }
                | ClientMsg::FetchAround { .. }) => self.handle_extra(me, msg).await,
                msg @ (ClientMsg::CreateCategory { .. }
                | ClientMsg::UpdateCategory { .. }
                | ClientMsg::DeleteCategory { .. }
                | ClientMsg::UpdateChannel { .. }
                | ClientMsg::ReorderChannels { .. }
                | ClientMsg::CreateThread { .. }
                | ClientMsg::ListThreads { .. }
                | ClientMsg::UpdateThread { .. }
                | ClientMsg::DeleteThread { .. }
                | ClientMsg::CreateGroup { .. }
                | ClientMsg::AddGroupMember { .. }
                | ClientMsg::RemoveGroupMember { .. }
                | ClientMsg::RenameGroup { .. }
                | ClientMsg::LeaveGroup { .. }
                | ClientMsg::SendPoll { .. }
                | ClientMsg::Vote { .. }
                | ClientMsg::EndPoll { .. }) => self.handle_social(me, msg).await,
                msg @ (ClientMsg::RenameEmoji { .. }
                | ClientMsg::DeleteEmoji { .. }
                | ClientMsg::RenameSticker { .. }
                | ClientMsg::DeleteSticker { .. }
                | ClientMsg::SendSticker { .. }
                | ClientMsg::UpdateSound { .. }
                | ClientMsg::DeleteSound { .. }
                | ClientMsg::PlaySound { .. }) => self.handle_sounds(me, msg).await,
                msg @ (ClientMsg::CreateInvite { .. }
                | ClientMsg::ListInvites
                | ClientMsg::RevokeInvite { .. }
                | ClientMsg::KickMember { .. }
                | ClientMsg::BanMember { .. }
                | ClientMsg::UnbanMember { .. }
                | ClientMsg::PublishIdentityKeys { .. }
                | ClientMsg::FetchIdentityKeys { .. }
                | ClientMsg::SendEncrypted { .. }
                | ClientMsg::EditEncrypted { .. }
                | ClientMsg::CallSignal { .. }) => self.handle_members(me, msg).await,
                msg => self.handle_core(me, msg).await,
            }
        }
        .await;

        match result {
            Ok(flow) => flow,
            Err(e) => {
                warn!("database error: {e}");
                self.error("Server error, please try again").await
            }
        }
    }

    /// Store and announce a message (text, sticker, poll or encrypted), after checking the
    /// author may post in that channel.
    async fn post(
        &mut self,
        me: UserId,
        channel_id: ChannelId,
        content: String,
        attachments: Vec<String>,
        reply_to: Option<MessageId>,
        extras: db::MessageExtras,
    ) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        let Some((kind, to)) = audience(&self.state, me, channel_id).await? else {
            return Ok(self.error("That channel no longer exists").await);
        };
        if !(is_server_text(kind) || is_conversation(kind)) {
            return Ok(self.error("You can't send messages there").await);
        }
        let encrypted = extras.encrypted.is_some();
        if encrypted && !is_conversation(kind) {
            return Ok(self.error("Encrypted messages are only for direct messages and groups").await);
        }
        let has_files = !attachments.is_empty();
        let refusal = db
            .call(move |c| {
                Ok(match kind {
                    ChannelKind::Direct => {
                        let other = db::direct_members(c, channel_id)?.into_iter().find(|u| *u != me);
                        match other {
                            Some(o) if db::blocked_between(c, me, o)? => {
                                Some("You can't message this person right now.")
                            }
                            _ => None,
                        }
                    }
                    ChannelKind::Group => None,
                    _ => {
                        let access = db::access(c, me)?;
                        if !access.can(perms::SEND_MESSAGES) {
                            Some("You don't have permission to send messages here.")
                        } else if has_files && !access.can(perms::ATTACH_FILES) {
                            Some("You don't have permission to send files here.")
                        } else {
                            None
                        }
                    }
                })
            })
            .await?;
        if let Some(why) = refusal {
            return Ok(self.error(why).await);
        }
        let sent = db
            .call(move |c| {
                let sent = db::insert_message_with(c, channel_id, me, &content, &attachments, reply_to, extras)?;
                let Ok(m) = sent else { return Ok(Err(())) };
                db::mark_read(c, me, channel_id, m.id)?;
                // Posting in an archived thread brings it back.
                let mut thread = None;
                if kind == ChannelKind::Thread
                    && let Some(t) = db::get_thread(c, channel_id)?
                {
                    if t.archived {
                        db::set_thread(c, channel_id, &t.name, false)?;
                    }
                    thread = db::get_thread(c, channel_id)?;
                }
                if encrypted {
                    return Ok(Ok((m, false, thread)));
                }
                let pinged = db::resolve_mentions(c, me, channel_id, &m.content)?;
                db::set_mentions(c, m.id, channel_id, &pinged)?;
                let embeds = is_conversation(kind) || db::access(c, me)?.can(perms::EMBED_LINKS);
                Ok(Ok((Message { mentions: pinged, ..m }, embeds, thread)))
            })
            .await?;
        match sent {
            Ok((msg, embeds, thread)) => {
                if embeds {
                    crate::previews::update(self.state.clone(), msg.clone(), None);
                }
                self.broadcast_target(&ServerMsg::MessageCreated(msg), to.clone());
                if let Some(t) = thread {
                    self.broadcast_target(&ServerMsg::ThreadUpdated(t), to);
                }
            }
            Err(()) => {
                return Ok(self.error("One of the files couldn't be attached. Try uploading it again.").await);
            }
        }
        Ok(Flow::Continue)
    }

    /// A thread's message count changed: tell everyone who sees it.
    async fn thread_changed(&mut self, channel: ChannelId) -> Result<(), rusqlite::Error> {
        if let Some(t) = self.state.db.call(move |c| db::get_thread(c, channel)).await? {
            self.broadcast_in(channel, &ServerMsg::ThreadUpdated(t)).await?;
        }
        Ok(())
    }

    /// Requests that existed before 0.5.
    async fn handle_core(&mut self, me: UserId, msg: ClientMsg) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        match msg {
            ClientMsg::Ping => {
                self.send(&ServerMsg::Pong).await;
            }
            ClientMsg::Register { .. } | ClientMsg::Login { .. } | ClientMsg::Resume { .. } => {
                return Ok(self.error("Already logged in").await);
            }
            ClientMsg::Logout => {
                if let Some(token) = self.token.take() {
                    db.call(move |c| db::delete_session(c, &token)).await?;
                }
                return Ok(Flow::Close);
            }
            ClientMsg::SendMessage { channel_id, content, attachments, reply_to } => {
                let content = content.trim().to_owned();
                if content.is_empty() && attachments.is_empty() {
                    return Ok(Flow::Continue);
                }
                if content.chars().count() > MAX_MESSAGE_LEN {
                    return Ok(self.error(format!("Messages can be at most {MAX_MESSAGE_LEN} characters")).await);
                }
                if attachments.len() > MAX_ATTACHMENTS {
                    return Ok(self.error(format!("You can attach at most {MAX_ATTACHMENTS} files")).await);
                }
                return self.post(me, channel_id, content, attachments, reply_to, db::MessageExtras::default()).await;
            }
            ClientMsg::EditMessage { message_id, content } => {
                let content = content.trim().to_owned();
                if content.chars().count() > MAX_MESSAGE_LEN {
                    return Ok(self.error("Invalid message").await);
                }
                let updated = db
                    .call(move |c| match db::get_message(c, message_id)? {
                        // A message needs text unless it has files or a sticker. Polls and
                        // encrypted messages aren't edited as text.
                        Some(m)
                            if m.author_id == me
                                && m.kind.is_normal()
                                && m.poll.is_none()
                                && m.encrypted.is_none()
                                && db::accessible_kind(c, me, m.channel_id)?.is_some()
                                && (!content.is_empty() || !m.attachments.is_empty() || m.sticker.is_some()) =>
                        {
                            db::edit_message(c, message_id, &content)?;
                            let pinged = db::resolve_mentions(c, me, m.channel_id, &content)?;
                            db::set_mentions(c, message_id, m.channel_id, &pinged)?;
                            Ok(db::get_message(c, message_id)?.map(|new| (new, m.content)))
                        }
                        _ => Ok(None),
                    })
                    .await?;
                match updated {
                    Some((m, before)) => {
                        crate::previews::update(self.state.clone(), m.clone(), Some(&before));
                        let channel = m.channel_id;
                        self.broadcast_in(channel, &ServerMsg::MessageUpdated(m)).await?;
                    }
                    None => return Ok(self.error("You can only edit your own messages").await),
                }
            }
            ClientMsg::DeleteMessage { message_id } => {
                let deleted = db
                    .call(move |c| {
                        let moderator = db::access(c, me)?.can(perms::MANAGE_MESSAGES);
                        let Some(m) = db::get_message(c, message_id)? else { return Ok(None) };
                        // Moderators can tidy up server channels, but never read or touch DMs.
                        let kind = db::accessible_kind(c, me, m.channel_id)?;
                        let allowed = m.author_id == me || (moderator && kind.is_some_and(is_server_text));
                        if !allowed || kind.is_none() {
                            return Ok(None);
                        }
                        let files = db::delete_message(c, message_id)?;
                        Ok(Some((m.channel_id, files, kind == Some(ChannelKind::Thread))))
                    })
                    .await?;
                match deleted {
                    Some((channel_id, files, in_thread)) => {
                        crate::http::remove_files(&self.state.config.uploads, &files).await;
                        self.broadcast_in(channel_id, &ServerMsg::MessageDeleted { channel_id, message_id }).await?;
                        if in_thread {
                            self.thread_changed(channel_id).await?;
                        }
                    }
                    None => return Ok(self.error("You can't delete that message").await),
                }
            }
            ClientMsg::FetchHistory { channel_id, before } => {
                if audience(&self.state, me, channel_id).await?.is_none() {
                    return Ok(self.error("You can't see that channel").await);
                }
                let messages = db.call(move |c| db::history(c, channel_id, before)).await?;
                let reached_start = messages.len() < HISTORY_PAGE;
                self.send(&ServerMsg::History { channel_id, messages, reached_start }).await;
            }
            ClientMsg::Typing { channel_id } => {
                if let Some((_, to)) = audience(&self.state, me, channel_id).await? {
                    self.broadcast_target(&ServerMsg::Typing { channel_id, user_id: me }, to);
                }
            }
            ClientMsg::AddReaction { message_id, emoji } | ClientMsg::RemoveReaction { message_id, emoji }
                if !valid_reaction(&emoji) =>
            {
                let _ = (message_id, emoji);
                return Ok(self.error("That isn't a valid reaction").await);
            }
            ClientMsg::AddReaction { message_id, emoji } => {
                let e = emoji.clone();
                let channel = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else { return Ok(None) };
                        match db::accessible_kind(c, me, m.channel_id)? {
                            None => return Ok(None),
                            Some(k) if is_conversation(k) => {}
                            Some(_) if !db::access(c, me)?.can(perms::ADD_REACTIONS) => return Ok(None),
                            Some(_) => {}
                        }
                        Ok(db::add_reaction(c, message_id, me, &e, MAX_REACTIONS)?.then_some(m.channel_id))
                    })
                    .await?;
                if let Some(channel_id) = channel {
                    let update = ServerMsg::ReactionUpdate { channel_id, message_id, emoji, user_id: me, added: true };
                    self.broadcast_in(channel_id, &update).await?;
                }
            }
            ClientMsg::RemoveReaction { message_id, emoji } => {
                let e = emoji.clone();
                let channel = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else { return Ok(None) };
                        Ok(db::remove_reaction(c, message_id, me, &e)?.then_some(m.channel_id))
                    })
                    .await?;
                if let Some(channel_id) = channel {
                    let update = ServerMsg::ReactionUpdate { channel_id, message_id, emoji, user_id: me, added: false };
                    self.broadcast_in(channel_id, &update).await?;
                }
            }
            ClientMsg::OpenDirect { user_id } => {
                if user_id == me {
                    return Ok(self.error("You can't message yourself").await);
                }
                let opened = db
                    .call(move |c| {
                        if db::get_user(c, user_id)?.is_none() {
                            return Ok(None);
                        }
                        if db::blocked_between(c, me, user_id)? {
                            return Ok(Some(None));
                        }
                        let (id, created) = db::open_direct(c, me, user_id)?;
                        Ok(Some(Some((id, created, db::last_message_id(c, id)?))))
                    })
                    .await?;
                let Some(opened) = opened else {
                    return Ok(self.error("That person doesn't exist").await);
                };
                let Some((id, created, last_message_id)) = opened else {
                    return Ok(self.error("You can't message this person right now.").await);
                };
                self.send(&ServerMsg::DirectOpened(DirectChannel { id, user_id, last_message_id })).await;
                if created {
                    let theirs = ServerMsg::DirectOpened(DirectChannel { id, user_id: me, last_message_id });
                    self.broadcast_to(&theirs, Some(Arc::from([user_id])));
                }
            }
            ClientMsg::SearchGifs { query } => {
                let Some(gifs) = &self.state.gifs else {
                    return Ok(self.error("GIF search isn't set up on this server").await);
                };
                match gifs.search(&query, me).await {
                    Ok(results) => {
                        self.send(&ServerMsg::GifResults { query, results }).await;
                    }
                    Err(e) => {
                        warn!("{e}");
                        return Ok(self.error("GIF search isn't working right now. Try again in a moment.").await);
                    }
                }
            }
            ClientMsg::SendGif { channel_id, gif_id } => {
                let Some(gifs) = &self.state.gifs else {
                    return Ok(self.error("GIF search isn't set up on this server").await);
                };
                let Some((url, title)) = gifs.lookup(&gif_id) else {
                    return Ok(self
                        .error("That GIF has expired. Search again and pick it from the new results.")
                        .await);
                };
                let Some((kind, _)) = audience(&self.state, me, channel_id).await? else {
                    return Ok(self.error("That channel no longer exists").await);
                };
                if !(is_server_text(kind) || is_conversation(kind)) {
                    return Ok(self.error("You can't send messages there").await);
                }
                let bytes = match gifs.download(&url).await {
                    Ok(b) => b,
                    Err(e) => return Ok(self.error(format!("Couldn't send that GIF: {e}")).await),
                };
                let name: String = title.chars().filter(|c| c.is_alphanumeric() || *c == ' ').take(40).collect();
                let name = format!("{}.gif", if name.trim().is_empty() { "gif" } else { name.trim() });
                let attachment = match crate::http::store_bytes(&self.state, me, channel_id, &name, &bytes).await {
                    Ok(a) => a,
                    Err((_, e)) => return Ok(self.error(format!("Couldn't send that GIF: {e}")).await),
                };
                return self
                    .post(me, channel_id, String::new(), vec![attachment.id], None, db::MessageExtras::default())
                    .await;
            }
            ClientMsg::MarkRead { channel_id, message_id } => {
                if db.call(move |c| db::accessible_kind(c, me, channel_id)).await?.is_some() {
                    db.call(move |c| db::mark_read(c, me, channel_id, message_id)).await?;
                }
            }
            ClientMsg::UpdateProfile { profile } => {
                let mut profile = profile.sanitized();
                let user = db
                    .call(move |c| {
                        // Pictures are only changed by uploading (or clearing) them.
                        let current = db::get_user(c, me)?.map(|u| u.profile).unwrap_or_default();
                        profile.avatar = current.avatar;
                        profile.banner = current.banner;
                        db::update_profile(c, me, &profile)?;
                        db::get_user(c, me)
                    })
                    .await?;
                if let Some(user) = user {
                    self.broadcast(&ServerMsg::UserUpdated(user));
                }
            }
            ClientMsg::CreateChannel { name, topic, kind, category_id } => {
                let name = match kind {
                    ChannelKind::Text => normalize_channel_name(&name),
                    ChannelKind::Voice => clean_voice_channel_name(&name),
                    ChannelKind::Direct | ChannelKind::Group => {
                        return Ok(self.error("Use direct messages for that").await);
                    }
                    ChannelKind::Thread => return Ok(self.error("Start threads from a text channel").await),
                };
                let topic: String = topic.trim().chars().take(120).collect();
                if name.is_empty() {
                    return Ok(self.error("Channel names need at least one letter or number").await);
                }
                let channel = db
                    .call(move |c| {
                        if !db::access(c, me)?.can(perms::MANAGE_CHANNELS) {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        if let Some(cat) = category_id
                            && db::get_category(c, cat)?.is_none()
                        {
                            return Ok(Err("That category no longer exists"));
                        }
                        let ch = db::create_channel(c, &name, &topic, kind)?;
                        if category_id.is_some() {
                            db::set_channel_category(c, ch.id, category_id)?;
                        }
                        Ok(Ok(db::get_channel(c, ch.id)?))
                    })
                    .await?;
                match channel {
                    Ok(Some(ch)) => {
                        let id = ch.id;
                        self.broadcast_in(id, &ServerMsg::ChannelCreated(ch)).await?;
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::DeleteChannel { channel_id } => {
                let outcome = db
                    .call(move |c| {
                        if !db::access(c, me)?.can(perms::MANAGE_CHANNELS) {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        let kind = db::channel_kind(c, channel_id)?;
                        if kind.is_some_and(is_conversation) {
                            return Ok(Err("Direct messages can't be deleted"));
                        }
                        if kind == Some(ChannelKind::Thread) {
                            return Ok(Err("Use Delete thread for threads"));
                        }
                        if kind == Some(ChannelKind::Text) && db::text_channel_count(c)? <= 1 {
                            return Ok(Err("You can't delete the last text channel"));
                        }
                        Ok(db::delete_channel(c, channel_id)?.ok_or("No such channel"))
                    })
                    .await?;
                match outcome {
                    Ok(files) => {
                        crate::http::remove_files(&self.state.config.uploads, &files).await;
                        for user_id in self.state.voice.clear_channel(channel_id) {
                            self.broadcast(&ServerMsg::VoiceStateUpdate { user_id, state: None });
                        }
                        self.broadcast(&ServerMsg::ChannelDeleted { channel_id });
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::JoinVoice { channel_id, encrypted } => {
                let refusal = db
                    .call(move |c| {
                        Ok(match db::accessible_kind(c, me, channel_id)? {
                            Some(ChannelKind::Voice) if !db::access(c, me)?.can(perms::CONNECT_VOICE) => {
                                Some("You don't have permission to join voice channels")
                            }
                            Some(ChannelKind::Voice) => None,
                            // A call in a direct message.
                            Some(ChannelKind::Direct) => {
                                let other = db::direct_members(c, channel_id)?.into_iter().find(|u| *u != me);
                                match other {
                                    Some(o) if db::blocked_between(c, me, o)? => {
                                        Some("You can't call this person right now")
                                    }
                                    _ => None,
                                }
                            }
                            // A group call: anyone in the group can join.
                            Some(ChannelKind::Group) => None,
                            _ => Some("That isn't a voice channel"),
                        })
                    })
                    .await?;
                if let Some(why) = refusal {
                    return Ok(self.error(why).await);
                }
                let previous = self.state.voice.channel_of(me);
                let mut joined =
                    self.state.voice.join(me, channel_id, self.conn_id, self.voice_tx.clone(), false, false, encrypted);
                if is_call(&db, channel_id).await?
                    && let Some(state) = self.state.voice.exempt(me)
                {
                    joined.state = state;
                }
                let udp_port = self.state.udp_port.load(Ordering::Relaxed);
                self.send(&ServerMsg::VoiceReady {
                    channel_id,
                    ssrc: joined.state.ssrc,
                    token: token_to_hex(&joined.token),
                    udp_port,
                    key: joined.key.as_ref().map(celerity_proto::voice::key_to_hex),
                })
                .await;
                // Moving out of a call or private channel: those who won't see where they went see them leave.
                if let Some(old) = previous.filter(|c| *c != channel_id) {
                    if let Some(to) = left_behind(&self.state, old, Some(channel_id)).await? {
                        self.broadcast_target(&ServerMsg::VoiceStateUpdate { user_id: me, state: None }, to);
                    }
                    crate::calls::left(&self.state, old).await;
                }
                self.broadcast_in(channel_id, &ServerMsg::VoiceStateUpdate { user_id: me, state: Some(joined.state) })
                    .await?;
                if previous != Some(channel_id) {
                    crate::calls::joined(&self.state, channel_id, me).await;
                }
            }
            ClientMsg::LeaveVoice => {
                let channel = self.state.voice.channel_of(me);
                let to = match channel {
                    Some(c) => self.state.channel_target(c).await?,
                    None => Target::everyone(),
                };
                if self.state.voice.leave(me, self.conn_id) {
                    self.broadcast_target(&ServerMsg::VoiceStateUpdate { user_id: me, state: None }, to);
                }
                if let Some(c) = channel {
                    crate::calls::left(&self.state, c).await;
                }
            }
            ClientMsg::SetVoiceState { muted, deafened } => {
                if let Some(state) = self.state.voice.set_state(me, self.conn_id, muted, deafened) {
                    let channel = state.channel_id;
                    self.broadcast_in(channel, &ServerMsg::VoiceStateUpdate { user_id: me, state: Some(state) })
                        .await?;
                }
            }
            ClientMsg::SetVideo { camera, screen } => {
                if let Some(state) = self.state.voice.set_video(me, self.conn_id, camera, screen) {
                    let channel = state.channel_id;
                    self.broadcast_in(channel, &ServerMsg::VoiceStateUpdate { user_id: me, state: Some(state) })
                        .await?;
                }
            }
            ClientMsg::WatchStream { user_id, watching } => {
                // A new viewer needs a keyframe to start from.
                if self.state.voice.watch(me, user_id, watching) {
                    self.broadcast_to(
                        &ServerMsg::KeyframeRequest { kind: celerity_proto::VideoKind::Screen },
                        Some(Arc::from([user_id])),
                    );
                }
            }
            ClientMsg::RequestKeyframe { user_id, kind } => {
                if self.state.voice.may_request_keyframe(me, user_id, kind) {
                    self.broadcast_to(&ServerMsg::KeyframeRequest { kind }, Some(Arc::from([user_id])));
                }
            }
            ClientMsg::MoveVoice { user_id, channel_id } => {
                // Only between server voice channels (not into or out of private calls),
                // and only into channels they can see.
                let from = self.state.voice.channel_of(user_id);
                let refusal = db
                    .call(move |c| {
                        let voice = |ch| Ok::<_, rusqlite::Error>(db::channel_kind(c, ch)? == Some(ChannelKind::Voice));
                        Ok(if !db::access(c, me)?.can(perms::MOVE_MEMBERS) {
                            Some("You don't have permission to move people")
                        } else if !db::outranks(c, me, user_id)? {
                            Some("You can't move someone ranked above you")
                        } else if !voice(channel_id)? || !from.map(voice).transpose()?.unwrap_or(false) {
                            Some("People can only be moved between voice channels")
                        } else if !db::can_see_channel(c, user_id, channel_id)?
                            || !db::can_see_channel(c, me, channel_id)?
                        {
                            Some("They can't see that channel")
                        } else {
                            None
                        })
                    })
                    .await?;
                if let Some(why) = refusal {
                    return Ok(self.error(why).await);
                }
                if let Some(state) = self.state.voice.move_member(user_id, channel_id) {
                    info!("user {me} moved user {user_id} to voice channel {channel_id}");
                    if let Some(old) = from
                        && let Some(to) = left_behind(&self.state, old, Some(channel_id)).await?
                    {
                        self.broadcast_target(&ServerMsg::VoiceStateUpdate { user_id, state: None }, to);
                    }
                    self.broadcast_in(channel_id, &ServerMsg::VoiceStateUpdate { user_id, state: Some(state) }).await?;
                }
            }
            ClientMsg::ModerateVoice { user_id, muted, deafened } => {
                // Only in server voice channels: private calls are nobody else's business.
                let in_channel = self.state.voice.channel_of(user_id);
                let refusal = db
                    .call(move |c| {
                        Ok(if !db::access(c, me)?.can(perms::MUTE_MEMBERS) {
                            Some("You don't have permission to mute people")
                        } else if !db::outranks(c, me, user_id)? {
                            Some("You can't mute someone ranked above you")
                        } else if let Some(ch) = in_channel
                            && db::channel_kind(c, ch)? != Some(ChannelKind::Voice)
                        {
                            Some("People can only be server-muted in voice channels")
                        } else {
                            None
                        })
                    })
                    .await?;
                if let Some(why) = refusal {
                    return Ok(self.error(why).await);
                }
                info!("user {me} set server mute={muted} deafen={deafened} on user {user_id}");
                if let Some(state) = self.state.voice.moderate(user_id, muted, deafened) {
                    let channel = state.channel_id;
                    self.broadcast_in(channel, &ServerMsg::VoiceStateUpdate { user_id, state: Some(state) }).await?;
                }
            }
            other => {
                warn!("unhandled request {other:?}");
            }
        }
        Ok(Flow::Continue)
    }

    async fn register(&mut self, username: String, password: String, invite: String) -> Flow {
        let username = username.trim().to_owned();
        let invite = invite.trim().to_owned();
        let required = &self.state.config.invite_code;
        // The server's invite code (or none at all); otherwise it has to be a working invite link.
        let by_code = required.is_empty() || invite == *required;
        if !valid_username(&username) {
            return self.auth_failed("Usernames are 2-32 characters: letters, numbers, _ and .").await;
        }
        if password.chars().count() < 8 {
            return self.auth_failed("Passwords need at least 8 characters").await;
        }
        const WRONG_INVITE: &str = "Wrong invite code, or the invite link has expired";

        let name = username.clone();
        let existing = match self.state.db.call(move |c| db::credentials(c, &name)).await {
            Ok(e) => e,
            Err(e) => {
                warn!("register failed: {e}");
                return self.auth_failed("Server error").await;
            }
        };
        if let Some(creds) = existing {
            // Someone who was kicked comes back by registering again with their own password.
            let pw = password.clone();
            let hash = creds.password_hash.clone();
            let theirs = tokio::task::spawn_blocking(move || verify_password(&pw, &hash)).await.expect("verify task");
            if !theirs || creds.removed == db::MEMBER {
                return self.auth_failed("That username is taken").await;
            }
            if creds.removed == db::BANNED {
                return self.auth_failed("You're banned from this server").await;
            }
            let id = creds.id;
            let back = self
                .state
                .db
                .call(move |c| {
                    if !by_code && !db::use_invite(c, &invite)? {
                        return Ok(None);
                    }
                    db::set_membership(c, id, db::MEMBER)?;
                    db::get_user(c, id)
                })
                .await;
            return match back {
                Ok(Some(user)) => {
                    info!("{} rejoined", user.username);
                    self.broadcast(&ServerMsg::UserUpdated(user));
                    self.start_session(id).await
                }
                Ok(None) => self.auth_failed(WRONG_INVITE).await,
                Err(e) => {
                    warn!("rejoin failed: {e}");
                    self.auth_failed("Server error").await
                }
            };
        }

        let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.expect("hash task");
        let name = username.clone();
        let created = self
            .state
            .db
            .call(move |c| {
                if !by_code && !db::use_invite(c, &invite)? {
                    return Ok(Err(WRONG_INVITE));
                }
                if db::credentials(c, &name)?.is_some() {
                    if !by_code {
                        db::unuse_invite(c, &invite)?;
                    }
                    return Ok(Err("That username is taken"));
                }
                // The very first account owns the server.
                let first = db::user_count(c)? == 0;
                db::create_user(c, &name, &hash, first).map(Ok)
            })
            .await;

        match created {
            Ok(Ok(user)) => {
                info!("new account: {}", user.username);
                self.broadcast(&ServerMsg::UserJoined(user.clone()));
                self.start_session(user.id).await
            }
            Ok(Err(why)) => self.auth_failed(why).await,
            Err(e) => {
                warn!("register failed: {e}");
                self.auth_failed("Server error").await
            }
        }
    }

    async fn login(&mut self, username: String, password: String) -> Flow {
        let name = username.trim().to_owned();
        let creds = match self.state.db.call(move |c| db::credentials(c, &name)).await {
            Ok(c) => c,
            Err(e) => {
                warn!("login lookup failed: {e}");
                return self.auth_failed("Server error").await;
            }
        };
        let ok_user = match creds {
            Some(creds) => tokio::task::spawn_blocking(move || {
                verify_password(&password, &creds.password_hash).then_some((creds.id, creds.removed))
            })
            .await
            .expect("verify task"),
            None => None,
        };
        match ok_user {
            Some((_, db::BANNED)) => self.auth_failed("You're banned from this server").await,
            Some((_, db::KICKED)) => {
                self.auth_failed(
                    "You were removed from this server. To come back, register again with your username, \
                     password and a new invite.",
                )
                .await
            }
            Some((id, _)) => self.start_session(id).await,
            None => self.auth_failed("Wrong username or password").await,
        }
    }

    async fn resume(&mut self, token: String) -> Flow {
        let t = token.clone();
        match self.state.db.call(move |c| db::session_user(c, &t)).await {
            Ok(Some(id)) => {
                self.token = Some(token);
                self.enter(id).await
            }
            Ok(None) => self.auth_failed("Session expired, please log in again").await,
            Err(e) => {
                warn!("resume failed: {e}");
                self.auth_failed("Server error").await
            }
        }
    }

    async fn start_session(&mut self, id: UserId) -> Flow {
        let token = random_token(40);
        let t = token.clone();
        if let Err(e) = self.state.db.call(move |c| db::create_session(c, &t, id)).await {
            warn!("could not create session: {e}");
            return self.auth_failed("Server error").await;
        }
        self.token = Some(token);
        self.enter(id).await
    }

    /// Mark the user online and send them the initial state.
    async fn enter(&mut self, id: UserId) -> Flow {
        let snapshot = self
            .state
            .db
            .call(move |c| {
                let channels = db::channels_for(c, id)?;
                let visible: Vec<ChannelId> = channels.iter().map(|ch| ch.id).collect();
                let threads =
                    db::active_threads(c)?.into_iter().filter(|t| visible.contains(&t.parent_id)).collect::<Vec<_>>();
                Ok(Ready {
                    token: String::new(),
                    me: id,
                    community_name: String::new(),
                    users: db::all_users(c)?,
                    online: Vec::new(),
                    channels,
                    voice: Vec::new(),
                    directs: db::directs_for(c, id)?,
                    read_states: db::read_states(c, id)?,
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    gif_provider: None,
                    max_upload_mb: 0,
                    roles: db::all_roles(c)?,
                    blocked: db::blocked_by(c, id)?,
                    mentions: db::mention_counts(c, id)?,
                    server_time: 0,
                    presences: Vec::new(),
                    categories: db::categories_for(c, id)?,
                    threads,
                    groups: db::groups_for(c, id)?,
                    emojis: db::all_emojis(c)?,
                    stickers: db::all_stickers(c)?,
                    sounds: db::all_sounds(c)?,
                    identity_keys: db::all_identity_keys(c)?,
                    invite_links: true,
                    public_address: None,
                })
            })
            .await;
        let mut ready: Ready = match snapshot {
            Ok(s) => s,
            Err(e) => {
                warn!("snapshot failed: {e}");
                return self.auth_failed("Server error").await;
            }
        };

        self.user = Some(id);
        let first_connection = {
            let mut online = self.state.online.lock().unwrap();
            let n = online.entry(id).or_default();
            *n += 1;
            *n == 1
        };
        if first_connection && self.state.presence_of(id).status != Status::Invisible {
            self.broadcast(&self.state.presence_msg(id));
        }
        // Invisible people look offline (except to themselves).
        let connected: Vec<UserId> = self.state.online.lock().unwrap().keys().copied().collect();
        let visible = |u: &UserId| *u == id || self.state.presence_of(*u).status != Status::Invisible;
        ready.online = connected.iter().copied().filter(visible).collect();
        ready.presences = ready.online.iter().map(|u| self.state.presence_of(*u)).collect();

        // Calls in other people's conversations, and private channels, are private.
        let (channels, directs, groups) = (&ready.channels, &ready.directs, &ready.groups);
        ready.voice = self
            .state
            .voice
            .states()
            .into_iter()
            .filter(|v| {
                channels.iter().any(|c| c.id == v.channel_id)
                    || directs.iter().any(|d| d.id == v.channel_id)
                    || groups.iter().any(|g| g.id == v.channel_id)
            })
            .collect();
        ready.token = self.token.clone().unwrap_or_default();
        ready.community_name = self.state.config.community_name.clone();
        ready.gif_provider = self.state.gifs.as_ref().map(|g| g.name().to_string());
        ready.max_upload_mb = self.state.config.max_upload_mb;
        ready.server_time = db::now_ms();
        ready.public_address = Some(self.state.config.public_address.trim().to_string()).filter(|a| !a.is_empty());
        if self.send(&ServerMsg::Ready(ready)).await { Flow::Continue } else { Flow::Close }
    }

    fn leave_voice(&self, id: UserId) {
        // Called when a connection closes; a private call's members find out from its channel.
        let channel = self.state.voice.channel_of(id);
        let (state, conn_id) = (self.state.clone(), self.conn_id);
        if !state.voice.leave(id, conn_id) {
            return;
        }
        tokio::spawn(async move {
            let to = match channel {
                Some(c) => state.channel_target(c).await.unwrap_or_default(),
                None => Target::everyone(),
            };
            state.broadcast_target(&ServerMsg::VoiceStateUpdate { user_id: id, state: None }, to);
            if let Some(c) = channel {
                crate::calls::left(&state, c).await;
            }
        });
    }

    fn set_offline(&self, id: UserId) {
        let last_connection = {
            let mut online = self.state.online.lock().unwrap();
            match online.get_mut(&id) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    online.remove(&id);
                    true
                }
                None => false,
            }
        };
        if last_connection {
            self.broadcast(&ServerMsg::Presence { user_id: id, online: false, status: Status::Online, activity: None });
        }
    }
}

/// Channels (and categories) people gained or lost sight of since `before` was taken:
/// tell each of them, so their channel list stays right without reconnecting.
async fn announce_visibility(state: &AppState, before: db::Visibility) -> Result<(), rusqlite::Error> {
    let (after, channels, categories) =
        state.db.call(|c| Ok((db::visibility_map(c)?, db::all_channels(c)?, db::all_categories(c)?))).await?;
    let find = |id: &ChannelId| channels.iter().find(|c| c.id == *id).cloned();
    // Categories first, so the channels in them have somewhere to go.
    for (user, now) in &after.categories {
        let was = before.categories.get(user);
        for gained in now.iter().filter(|c| was.is_none_or(|w| !w.contains(c))) {
            if let Some(cat) = categories.iter().find(|c| c.id == *gained) {
                state.broadcast(&ServerMsg::CategoryUpdated(cat.clone()), Some(Arc::from([*user])));
            }
        }
    }
    for (user, now) in &after.channels {
        let was = before.channels.get(user);
        for gained in now.iter().filter(|c| was.is_none_or(|w| !w.contains(c))) {
            if let Some(ch) = find(gained) {
                state.broadcast(&ServerMsg::ChannelCreated(ch), Some(Arc::from([*user])));
            }
        }
        if let Some(was) = was {
            for lost in was.iter().filter(|c| !now.contains(c)) {
                state.broadcast(&ServerMsg::ChannelDeleted { channel_id: *lost }, Some(Arc::from([*user])));
                // Nobody stays in a voice channel they can no longer see.
                if state.voice.channel_of(*user) == Some(*lost) {
                    state.voice.remove_user(*user);
                    state.broadcast(&ServerMsg::VoiceStateUpdate { user_id: *user, state: None }, None);
                }
            }
        }
    }
    for (user, now) in &after.categories {
        if let Some(was) = before.categories.get(user) {
            for lost in was.iter().filter(|c| !now.contains(c)) {
                state.broadcast(&ServerMsg::CategoryDeleted { category_id: *lost }, Some(Arc::from([*user])));
            }
        }
    }
    Ok(())
}

/// The roles a channel or category is limited to, keeping only roles that exist.
fn existing_roles(all: &[Role], wanted: &[RoleId]) -> Vec<RoleId> {
    let mut out: Vec<RoleId> = wanted.iter().copied().filter(|r| all.iter().any(|x| x.id == *r)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords() {
        let h = hash_password("hunter22!");
        assert!(verify_password("hunter22!", &h));
        assert!(!verify_password("hunter23!", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn usernames() {
        assert!(valid_username("alice_99"));
        assert!(!valid_username("a"));
        assert!(!valid_username("has space"));
    }
}
