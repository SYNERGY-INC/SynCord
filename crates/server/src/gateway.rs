//! WebSocket gateway: authentication, then a loop that handles client
//! requests and forwards broadcast events.

use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use rand::Rng;
use syncord_proto::{ClientMsg, HISTORY_PAGE, MAX_MESSAGE_LEN, Ready, ServerMsg, User, UserId, normalize_channel_name};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::AppState;
use crate::db;

/// Close idle connections that haven't sent anything (not even a ping) for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Messages-per-window allowed before a client is told to slow down.
const RATE_BURST: u32 = 20;
const RATE_WINDOW: Duration = Duration::from_secs(10);

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| async move {
        Connection { state, socket, user: None, token: None, rate: RateLimit::new() }.run().await
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
}

/// What the main loop should do after handling a frame.
enum Flow {
    Continue,
    Close,
}

impl Connection {
    async fn run(mut self) {
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
                        Ok(json) => {
                            if self.socket.send(WsMessage::Text(json.as_ref().into())).await.is_err() {
                                break;
                            }
                        }
                        // The client fell too far behind; drop it so it reconnects and resyncs.
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }

        if let Some(id) = self.user {
            self.set_offline(id);
        }
    }

    async fn send(&mut self, msg: &ServerMsg) -> bool {
        let json = serde_json::to_string(msg).expect("serialize");
        self.socket.send(WsMessage::Text(json.into())).await.is_ok()
    }

    fn broadcast(&self, msg: &ServerMsg) {
        let json: Arc<str> = serde_json::to_string(msg).expect("serialize").into();
        // Err only means nobody is listening right now.
        let _ = self.state.events.send(json);
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
        if !self.rate.allow() {
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
                ClientMsg::SendMessage { channel_id, content } => {
                    let content = content.trim().to_owned();
                    if content.is_empty() {
                        return Ok(Flow::Continue);
                    }
                    if content.chars().count() > MAX_MESSAGE_LEN {
                        return Ok(self.error(format!("Messages can be at most {MAX_MESSAGE_LEN} characters")).await);
                    }
                    let msg = db
                        .call(move |c| {
                            if !db::channel_exists(c, channel_id)? {
                                return Ok(None);
                            }
                            db::insert_message(c, channel_id, me, &content).map(Some)
                        })
                        .await?;
                    match msg {
                        Some(msg) => self.broadcast(&ServerMsg::MessageCreated(msg)),
                        None => return Ok(self.error("That channel no longer exists").await),
                    }
                }
                ClientMsg::EditMessage { message_id, content } => {
                    let content = content.trim().to_owned();
                    if content.is_empty() || content.chars().count() > MAX_MESSAGE_LEN {
                        return Ok(self.error("Invalid message").await);
                    }
                    let updated = db
                        .call(move |c| match db::get_message(c, message_id)? {
                            Some(m) if m.author_id == me => {
                                db::edit_message(c, message_id, &content)?;
                                db::get_message(c, message_id)
                            }
                            _ => Ok(None),
                        })
                        .await?;
                    match updated {
                        Some(m) => self.broadcast(&ServerMsg::MessageUpdated(m)),
                        None => return Ok(self.error("You can only edit your own messages").await),
                    }
                }
                ClientMsg::DeleteMessage { message_id } => {
                    let deleted = db
                        .call(move |c| {
                            let is_owner = db::get_user(c, me)?.is_some_and(|u| u.is_owner);
                            match db::get_message(c, message_id)? {
                                Some(m) if m.author_id == me || is_owner => {
                                    db::delete_message(c, message_id)?;
                                    Ok(Some(m.channel_id))
                                }
                                _ => Ok(None),
                            }
                        })
                        .await?;
                    match deleted {
                        Some(channel_id) => self.broadcast(&ServerMsg::MessageDeleted { channel_id, message_id }),
                        None => return Ok(self.error("You can't delete that message").await),
                    }
                }
                ClientMsg::FetchHistory { channel_id, before } => {
                    let messages = db.call(move |c| db::history(c, channel_id, before)).await?;
                    let reached_start = messages.len() < HISTORY_PAGE;
                    self.send(&ServerMsg::History { channel_id, messages, reached_start }).await;
                }
                ClientMsg::Typing { channel_id } => {
                    self.broadcast(&ServerMsg::Typing { channel_id, user_id: me });
                }
                ClientMsg::UpdateProfile { profile } => {
                    let profile = profile.sanitized();
                    let user = db
                        .call(move |c| {
                            db::update_profile(c, me, &profile)?;
                            db::get_user(c, me)
                        })
                        .await?;
                    if let Some(user) = user {
                        self.broadcast(&ServerMsg::UserUpdated(user));
                    }
                }
                ClientMsg::CreateChannel { name, topic } => {
                    let name = normalize_channel_name(&name);
                    let topic: String = topic.trim().chars().take(120).collect();
                    if name.is_empty() {
                        return Ok(self.error("Channel names need at least one letter or number").await);
                    }
                    let channel = db
                        .call(move |c| {
                            if !db::get_user(c, me)?.is_some_and(|u| u.is_owner) {
                                return Ok(None);
                            }
                            db::create_channel(c, &name, &topic).map(Some)
                        })
                        .await?;
                    match channel {
                        Some(ch) => self.broadcast(&ServerMsg::ChannelCreated(ch)),
                        None => return Ok(self.error("Only the server owner can create channels").await),
                    }
                }
                ClientMsg::DeleteChannel { channel_id } => {
                    let outcome = db
                        .call(move |c| {
                            if !db::get_user(c, me)?.is_some_and(|u| u.is_owner) {
                                return Ok(Err("Only the server owner can delete channels"));
                            }
                            if db::channel_count(c)? <= 1 {
                                return Ok(Err("You can't delete the last channel"));
                            }
                            Ok(if db::delete_channel(c, channel_id)? { Ok(()) } else { Err("No such channel") })
                        })
                        .await?;
                    match outcome {
                        Ok(()) => self.broadcast(&ServerMsg::ChannelDeleted { channel_id }),
                        Err(e) => return Ok(self.error(e).await),
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
        let snapshot = self.state.db.call(|c| Ok((db::all_users(c)?, db::all_channels(c)?))).await;
        let (users, channels): (Vec<User>, _) = match snapshot {
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

        let ready = Ready {
            token: self.token.clone().unwrap_or_default(),
            me: id,
            community_name: self.state.config.community_name.clone(),
            users,
            online,
            channels,
        };
        if self.send(&ServerMsg::Ready(ready)).await { Flow::Continue } else { Flow::Close }
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
