//! WebSocket gateway: authentication, then a loop that handles client
//! requests and forwards broadcast events.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use rand::Rng;
use syncord_proto::voice::token_to_hex;
use syncord_proto::{
    Channel, ChannelId, ChannelKind, ClientMsg, DirectChannel, EVERYONE_ROLE, HISTORY_PAGE, MAX_ATTACHMENTS,
    MAX_MESSAGE_LEN, MAX_REACTIONS, MAX_ROLES, Message, Ready, Role, RoleId, ServerMsg, User, UserId,
    clean_voice_channel_name, normalize_channel_name, perms, valid_reaction,
};

mod extra;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::db;
use crate::{AppState, Event};

/// Close idle connections that haven't sent anything (not even a ping) for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Messages-per-window allowed before a client is told to slow down.
const RATE_BURST: u32 = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10);

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| async move {
        let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let (voice_tx, voice_rx) = mpsc::channel(64);
        Connection { state, socket, user: None, token: None, rate: RateLimit::new(), conn_id, voice_tx }
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

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

fn valid_username(name: &str) -> bool {
    (2..=32).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

struct RateLimit {
    window_start: Instant,
    count: u32,
}

impl RateLimit {
    fn new() -> Self {
        Self { window_start: Instant::now(), count: 0 }
    }

    fn allow(&mut self) -> bool {
        if self.window_start.elapsed() > RATE_WINDOW {
            self.window_start = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= RATE_BURST
    }
}

struct Connection {
    state: Arc<AppState>,
    socket: WebSocket,
    user: Option<UserId>,
    token: Option<String>,
    rate: RateLimit,
    conn_id: u64,
    /// Voice packets the relay wants delivered over this WebSocket.
    voice_tx: mpsc::Sender<Bytes>,
}

/// If `me` can see `channel`: its kind and who should get its events
/// (`None` = everyone; a direct message's two members otherwise).
async fn audience(
    db: &db::Db,
    me: UserId,
    channel: ChannelId,
) -> Result<Option<(ChannelKind, Option<Arc<[UserId]>>)>, rusqlite::Error> {
    db.call(move |c| {
        Ok(match db::accessible_kind(c, me, channel)? {
            None => None,
            Some(ChannelKind::Direct) => Some((ChannelKind::Direct, Some(Arc::from(db::direct_members(c, channel)?)))),
            Some(kind) => Some((kind, None)),
        })
    })
    .await
}

/// What the main loop should do after handling a frame.
enum Flow {
    Continue,
    Close,
}

/// Who hears about a voice channel's comings and goings: the two members of a
/// direct-message call, or everyone for server voice channels.
async fn voice_audience(db: &db::Db, channel: ChannelId) -> Result<Option<Arc<[UserId]>>, rusqlite::Error> {
    db.call(move |c| match db::channel_kind(c, channel)? {
        Some(ChannelKind::Direct) => Ok(Some(Arc::from(db::direct_members(c, channel)?))),
        _ => Ok(None),
    })
    .await
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
                            if self.user.is_some_and(|me| event.is_for(me))
                                && self.socket.send(WsMessage::Text(event.json.as_ref().into())).await.is_err()
                            {
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
        let json: Arc<str> = serde_json::to_string(msg).expect("serialize").into();
        // Err only means nobody is listening right now.
        let _ = self.state.events.send(Event { json, to });
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
        // Read markers are cheap and sent automatically, so they don't count towards the limit.
        if !matches!(msg, ClientMsg::MarkRead { .. }) && !self.rate.allow() {
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

        let db = self.state.db.clone();
        let result: Result<Flow, rusqlite::Error> = async {
            match msg {
                msg @ (ClientMsg::CreateRole { .. }
                | ClientMsg::UpdateRole(_)
                | ClientMsg::DeleteRole { .. }
                | ClientMsg::SetUserRoles { .. }
                | ClientMsg::SetBlocked { .. }
                | ClientMsg::ClearProfileImage { .. }) => return self.handle_extra(me, msg).await,
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
                    let Some((kind, to)) = audience(&db, me, channel_id).await? else {
                        return Ok(self.error("That channel no longer exists").await);
                    };
                    if !matches!(kind, ChannelKind::Text | ChannelKind::Direct) {
                        return Ok(self.error("You can't send messages there").await);
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
                            let sent = db::insert_message(c, channel_id, me, &content, &attachments, reply_to)?;
                            if let Ok(m) = &sent {
                                db::mark_read(c, me, channel_id, m.id)?;
                                let pinged = db::resolve_mentions(c, me, channel_id, &m.content)?;
                                db::set_mentions(c, m.id, channel_id, &pinged)?;
                                let embeds = kind == ChannelKind::Direct || db::access(c, me)?.can(perms::EMBED_LINKS);
                                return Ok(Ok((Message { mentions: pinged, ..m.clone() }, embeds)));
                            }
                            Ok(sent.map(|m| (m, false)))
                        })
                        .await?;
                    match sent {
                        Ok((msg, embeds)) => {
                            if embeds {
                                crate::previews::update(self.state.clone(), msg.clone(), None);
                            }
                            self.broadcast_to(&ServerMsg::MessageCreated(msg), to)
                        }
                        Err(db::SendError::BadAttachment) => {
                            return Ok(self
                                .error("One of the files couldn't be attached. Try uploading it again.")
                                .await);
                        }
                    }
                }
                ClientMsg::EditMessage { message_id, content } => {
                    let content = content.trim().to_owned();
                    if content.chars().count() > MAX_MESSAGE_LEN {
                        return Ok(self.error("Invalid message").await);
                    }
                    let updated = db
                        .call(move |c| match db::get_message(c, message_id)? {
                            // A message needs text unless it has files.
                            Some(m) if m.author_id == me && (!content.is_empty() || !m.attachments.is_empty()) => {
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
                            let to = audience(&db, me, m.channel_id).await?.and_then(|(_, to)| to);
                            self.broadcast_to(&ServerMsg::MessageUpdated(m), to);
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
                            let allowed = m.author_id == me || (moderator && kind == Some(ChannelKind::Text));
                            if !allowed || kind.is_none() {
                                return Ok(None);
                            }
                            let files = db::delete_message(c, message_id)?;
                            Ok(Some((m.channel_id, files)))
                        })
                        .await?;
                    match deleted {
                        Some((channel_id, files)) => {
                            crate::http::remove_files(&self.state.config.uploads, &files).await;
                            let to = audience(&db, me, channel_id).await?.and_then(|(_, to)| to);
                            self.broadcast_to(&ServerMsg::MessageDeleted { channel_id, message_id }, to);
                        }
                        None => return Ok(self.error("You can't delete that message").await),
                    }
                }
                ClientMsg::FetchHistory { channel_id, before } => {
                    if audience(&db, me, channel_id).await?.is_none() {
                        return Ok(self.error("You can't see that channel").await);
                    }
                    let messages = db.call(move |c| db::history(c, channel_id, before)).await?;
                    let reached_start = messages.len() < HISTORY_PAGE;
                    self.send(&ServerMsg::History { channel_id, messages, reached_start }).await;
                }
                ClientMsg::Typing { channel_id } => {
                    if let Some((_, to)) = audience(&db, me, channel_id).await? {
                        self.broadcast_to(&ServerMsg::Typing { channel_id, user_id: me }, to);
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
                                Some(ChannelKind::Direct) => {}
                                Some(_) if !db::access(c, me)?.can(perms::ADD_REACTIONS) => return Ok(None),
                                Some(_) => {}
                            }
                            Ok(db::add_reaction(c, message_id, me, &e, MAX_REACTIONS)?.then_some(m.channel_id))
                        })
                        .await?;
                    if let Some(channel_id) = channel {
                        let to = audience(&db, me, channel_id).await?.and_then(|(_, to)| to);
                        let update =
                            ServerMsg::ReactionUpdate { channel_id, message_id, emoji, user_id: me, added: true };
                        self.broadcast_to(&update, to);
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
                        let to = audience(&db, me, channel_id).await?.and_then(|(_, to)| to);
                        let update =
                            ServerMsg::ReactionUpdate { channel_id, message_id, emoji, user_id: me, added: false };
                        self.broadcast_to(&update, to);
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
                    let Some((kind, to)) = audience(&db, me, channel_id).await? else {
                        return Ok(self.error("That channel no longer exists").await);
                    };
                    if !matches!(kind, ChannelKind::Text | ChannelKind::Direct) {
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
                    let ids = vec![attachment.id];
                    let sent = db
                        .call(move |c| {
                            let sent = db::insert_message(c, channel_id, me, "", &ids, None)?;
                            if let Ok(m) = &sent {
                                db::mark_read(c, me, channel_id, m.id)?;
                            }
                            Ok(sent)
                        })
                        .await?;
                    if let Ok(msg) = sent {
                        self.broadcast_to(&ServerMsg::MessageCreated(msg), to);
                    }
                }
                ClientMsg::MarkRead { channel_id, message_id } => {
                    if audience(&db, me, channel_id).await?.is_some() {
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
                ClientMsg::CreateChannel { name, topic, kind } => {
                    let name = match kind {
                        ChannelKind::Text => normalize_channel_name(&name),
                        ChannelKind::Voice => clean_voice_channel_name(&name),
                        ChannelKind::Direct => return Ok(self.error("Use direct messages for that").await),
                    };
                    let topic: String = topic.trim().chars().take(120).collect();
                    if name.is_empty() {
                        return Ok(self.error("Channel names need at least one letter or number").await);
                    }
                    let channel = db
                        .call(move |c| {
                            if !db::access(c, me)?.can(perms::MANAGE_CHANNELS) {
                                return Ok(None);
                            }
                            db::create_channel(c, &name, &topic, kind).map(Some)
                        })
                        .await?;
                    match channel {
                        Some(ch) => self.broadcast(&ServerMsg::ChannelCreated(ch)),
                        None => return Ok(self.error("You need the Manage channels permission").await),
                    }
                }
                ClientMsg::DeleteChannel { channel_id } => {
                    let outcome = db
                        .call(move |c| {
                            if !db::access(c, me)?.can(perms::MANAGE_CHANNELS) {
                                return Ok(Err("You need the Manage channels permission"));
                            }
                            let kind = db::channel_kind(c, channel_id)?;
                            if kind == Some(ChannelKind::Direct) {
                                return Ok(Err("Direct messages can't be deleted"));
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
                                _ => Some("That isn't a voice channel"),
                            })
                        })
                        .await?;
                    if let Some(why) = refusal {
                        return Ok(self.error(why).await);
                    }
                    let previous = self.state.voice.channel_of(me);
                    let joined = self.state.voice.join(
                        me,
                        channel_id,
                        self.conn_id,
                        self.voice_tx.clone(),
                        false,
                        false,
                        encrypted,
                    );
                    let udp_port = self.state.udp_port.load(Ordering::Relaxed);
                    self.send(&ServerMsg::VoiceReady {
                        channel_id,
                        ssrc: joined.state.ssrc,
                        token: token_to_hex(&joined.token),
                        udp_port,
                        key: joined.key.as_ref().map(syncord_proto::voice::key_to_hex),
                    })
                    .await;
                    // Moving out of a private call: its members see you leave.
                    if let Some(old) = previous.filter(|c| *c != channel_id) {
                        let to = voice_audience(&db, old).await?;
                        if to.is_some() {
                            self.broadcast_to(&ServerMsg::VoiceStateUpdate { user_id: me, state: None }, to);
                        }
                    }
                    let to = voice_audience(&db, channel_id).await?;
                    self.broadcast_to(&ServerMsg::VoiceStateUpdate { user_id: me, state: Some(joined.state) }, to);
                }
                ClientMsg::LeaveVoice => {
                    let channel = self.state.voice.channel_of(me);
                    let to = match channel {
                        Some(c) => voice_audience(&db, c).await?,
                        None => None,
                    };
                    self.leave_voice_to(me, to);
                }
                ClientMsg::SetVoiceState { muted, deafened } => {
                    if let Some(state) = self.state.voice.set_state(me, self.conn_id, muted, deafened) {
                        let to = voice_audience(&db, state.channel_id).await?;
                        self.broadcast_to(&ServerMsg::VoiceStateUpdate { user_id: me, state: Some(state) }, to);
                    }
                }
            }
            Ok(Flow::Continue)
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

    async fn register(&mut self, username: String, password: String, invite: String) -> Flow {
        let username = username.trim().to_owned();
        let required = &self.state.config.invite_code;
        if !required.is_empty() && invite.trim() != required {
            return self.auth_failed("Wrong invite code").await;
        }
        if !valid_username(&username) {
            return self.auth_failed("Usernames are 2-32 characters: letters, numbers, _ and .").await;
        }
        if password.chars().count() < 8 {
            return self.auth_failed("Passwords need at least 8 characters").await;
        }

        let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.expect("hash task");
        let name = username.clone();
        let created = self
            .state
            .db
            .call(move |c| {
                if db::credentials(c, &name)?.is_some() {
                    return Ok(None);
                }
                // The very first account owns the server.
                let first = db::user_count(c)? == 0;
                db::create_user(c, &name, &hash, first).map(Some)
            })
            .await;

        match created {
            Ok(Some(user)) => {
                info!("new account: {}", user.username);
                self.broadcast(&ServerMsg::UserJoined(user.clone()));
                self.start_session(user.id).await
            }
            Ok(None) => self.auth_failed("That username is taken").await,
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
                verify_password(&password, &creds.password_hash).then_some(creds.id)
            })
            .await
            .expect("verify task"),
            None => None,
        };
        match ok_user {
            Some(id) => self.start_session(id).await,
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
                Ok((
                    db::all_users(c)?,
                    db::all_channels(c)?,
                    db::directs_for(c, id)?,
                    db::read_states(c, id)?,
                    (db::all_roles(c)?, db::blocked_by(c, id)?, db::mention_counts(c, id)?),
                ))
            })
            .await;
        let (users, channels, directs, read_states, (roles, blocked, mentions)): (
            Vec<User>,
            Vec<Channel>,
            Vec<DirectChannel>,
            _,
            _,
        ) = match snapshot {
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
        if first_connection {
            self.broadcast(&ServerMsg::Presence { user_id: id, online: true });
        }
        let online = self.state.online.lock().unwrap().keys().copied().collect();

        // Calls in other people's direct messages are private.
        let voice = self
            .state
            .voice
            .states()
            .into_iter()
            .filter(|v| channels.iter().any(|c| c.id == v.channel_id) || directs.iter().any(|d| d.id == v.channel_id))
            .collect();
        let ready = Ready {
            token: self.token.clone().unwrap_or_default(),
            me: id,
            community_name: self.state.config.community_name.clone(),
            users,
            online,
            channels,
            voice,
            directs,
            read_states,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            gif_provider: self.state.gifs.as_ref().map(|g| g.name().to_string()),
            max_upload_mb: self.state.config.max_upload_mb,
            roles,
            blocked,
            mentions,
        };
        if self.send(&ServerMsg::Ready(ready)).await { Flow::Continue } else { Flow::Close }
    }

    fn leave_voice(&self, id: UserId) {
        // Called when a connection closes; a private call's members find out from its channel.
        let channel = self.state.voice.channel_of(id);
        let (db, state, conn_id) = (self.state.db.clone(), self.state.clone(), self.conn_id);
        if !state.voice.leave(id, conn_id) {
            return;
        }
        tokio::spawn(async move {
            let to = match channel {
                Some(c) => voice_audience(&db, c).await.ok().flatten(),
                None => None,
            };
            state.broadcast(&ServerMsg::VoiceStateUpdate { user_id: id, state: None }, to);
        });
    }

    fn leave_voice_to(&self, id: UserId, to: Option<Arc<[UserId]>>) {
        if self.state.voice.leave(id, self.conn_id) {
            self.broadcast_to(&ServerMsg::VoiceStateUpdate { user_id: id, state: None }, to);
        }
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
            self.broadcast(&ServerMsg::Presence { user_id: id, online: false });
        }
    }
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
