//! SQLite storage. A single connection behind a mutex is plenty for a friend
//! group and keeps memory use tiny; all calls run on tokio's blocking pool.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use syncord_proto::{
    Attachment, Channel, ChannelId, ChannelKind, DirectChannel, EVERYONE_ROLE, Embed, HISTORY_PAGE, MentionCount,
    Message, MessageId, MessageKind, Profile, Reaction, ReadState, Role, RoleId, SEARCH_PAGE, SearchHas, SearchQuery,
    User, UserId, parse_mentions, perms,
};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub type DbResult<T> = Result<T, rusqlite::Error>;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    profile       TEXT NOT NULL DEFAULT '{}',
    is_owner      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    token      TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS channels (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    topic      TEXT NOT NULL DEFAULT '',
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_id  INTEGER NOT NULL REFERENCES users(id),
    content    TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    edited_at  INTEGER
);

CREATE INDEX IF NOT EXISTS messages_by_channel ON messages(channel_id, id);

CREATE TABLE IF NOT EXISTS direct_members (
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX IF NOT EXISTS direct_members_by_user ON direct_members(user_id);

CREATE TABLE IF NOT EXISTS attachments (
    id          TEXT PRIMARY KEY,
    uploader_id INTEGER NOT NULL REFERENCES users(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    message_id  INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    filename    TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    width       INTEGER,
    height      INTEGER,
    animated    INTEGER NOT NULL DEFAULT 0,
    has_thumb   INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_by_message ON attachments(message_id);

CREATE TABLE IF NOT EXISTS reactions (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    emoji      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, user_id, emoji)
);

CREATE TABLE IF NOT EXISTS roles (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    color       INTEGER,
    permissions INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    hoist       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS user_roles (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id INTEGER NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, role_id)
);

CREATE TABLE IF NOT EXISTS blocks (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blocked_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, blocked_id)
);

CREATE TABLE IF NOT EXISTS mentions (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, user_id)
);
CREATE INDEX IF NOT EXISTS mentions_by_user ON mentions(user_id, channel_id, message_id);

CREATE TABLE IF NOT EXISTS profile_images (
    id         TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mime       TEXT NOT NULL,
    animated   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS read_states (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    last_read  INTEGER NOT NULL,
    PRIMARY KEY (user_id, channel_id)
);
"#;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Stored credentials for a user, used during login.
pub struct Credentials {
    pub id: UserId,
    pub password_hash: String,
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.seed()?;
        Ok(db)
    }

    /// A throwaway database, used by tests.
    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.seed()?;
        Ok(db)
    }

    fn seed(&self) -> DbResult<()> {
        let conn = self.lock();
        end_stale_calls(&conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO roles (id, name, color, permissions, position) VALUES (?1, '@everyone', NULL, ?2, 0)",
            params![EVERYONE_ROLE, perms::DEFAULT as i64],
        )?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))?;
        if count == 0 {
            create_channel(&conn, "general", "Say hi!", ChannelKind::Text)?;
            create_channel(&conn, "General", "", ChannelKind::Voice)?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock can't leave SQLite in a bad state,
        // so recover from poisoning instead of taking the server down.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run a closure against the connection on the blocking thread pool.
    pub async fn call<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || f(&db.lock())).await.expect("database task panicked")
    }
}

/// Bring databases created by older versions up to date.
fn migrate(conn: &Connection) -> DbResult<()> {
    let has_kind = conn.prepare("SELECT kind FROM channels LIMIT 0").is_ok();
    if !has_kind {
        conn.execute_batch("ALTER TABLE channels ADD COLUMN kind INTEGER NOT NULL DEFAULT 0")?;
    }
    // 0.5: link previews, videos, and pictures that belong to a link preview.
    for (table, column, ddl) in [
        ("messages", "embeds", "ALTER TABLE messages ADD COLUMN embeds TEXT NOT NULL DEFAULT '[]'"),
        ("attachments", "video", "ALTER TABLE attachments ADD COLUMN video TEXT"),
        ("attachments", "is_embed", "ALTER TABLE attachments ADD COLUMN is_embed INTEGER NOT NULL DEFAULT 0"),
        ("messages", "kind", "ALTER TABLE messages ADD COLUMN kind TEXT"),
    ] {
        if conn.prepare(&format!("SELECT {column} FROM {table} LIMIT 0")).is_err() {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

fn kind_to_i64(kind: ChannelKind) -> i64 {
    match kind {
        ChannelKind::Text => 0,
        ChannelKind::Voice => 1,
        ChannelKind::Direct => 2,
    }
}

fn kind_from_i64(v: i64) -> ChannelKind {
    match v {
        1 => ChannelKind::Voice,
        2 => ChannelKind::Direct,
        _ => ChannelKind::Text,
    }
}

// ---- Queries (synchronous, called inside `Db::call`) ----

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    let profile_json: String = row.get(2)?;
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        profile: serde_json::from_str(&profile_json).unwrap_or_default(),
        is_owner: row.get(3)?,
        roles: Vec::new(),
    })
}

pub fn user_count(conn: &Connection) -> DbResult<i64> {
    conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
}

pub fn create_user(conn: &Connection, username: &str, password_hash: &str, is_owner: bool) -> DbResult<User> {
    let profile = Profile { accent_color: color_for(username), ..Profile::default() };
    let profile = Profile { banner_color: profile.accent_color, ..profile };
    conn.execute(
        "INSERT INTO users (username, password_hash, profile, is_owner, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![username, password_hash, serde_json::to_string(&profile).unwrap(), is_owner, now_ms()],
    )?;
    Ok(User { id: conn.last_insert_rowid(), username: username.to_owned(), profile, is_owner, roles: Vec::new() })
}

/// Pick a pleasant default accent colour from the username.
fn color_for(username: &str) -> u32 {
    const COLORS: [u32; 8] = [0x5865F2, 0x3BA55C, 0xFAA61A, 0xED4245, 0xEB459E, 0x1ABC9C, 0x9B59B6, 0xE67E22];
    let hash = username.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLORS[hash as usize % COLORS.len()]
}

pub fn credentials(conn: &Connection, username: &str) -> DbResult<Option<Credentials>> {
    conn.query_row("SELECT id, password_hash FROM users WHERE username = ?1", params![username], |r| {
        Ok(Credentials { id: r.get(0)?, password_hash: r.get(1)? })
    })
    .optional()
}

pub fn get_user(conn: &Connection, id: UserId) -> DbResult<Option<User>> {
    let user = conn
        .query_row("SELECT id, username, profile, is_owner FROM users WHERE id = ?1", params![id], row_to_user)
        .optional()?;
    Ok(match user {
        Some(mut u) => {
            u.roles = user_roles(conn, u.id)?;
            Some(u)
        }
        None => None,
    })
}

pub fn all_users(conn: &Connection) -> DbResult<Vec<User>> {
    let mut stmt = conn.prepare_cached("SELECT id, username, profile, is_owner FROM users ORDER BY id")?;
    let mut users: Vec<User> = stmt.query_map([], row_to_user)?.collect::<DbResult<_>>()?;
    let mut roles: HashMap<UserId, Vec<RoleId>> = HashMap::new();
    let mut stmt = conn.prepare_cached("SELECT user_id, role_id FROM user_roles ORDER BY role_id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, UserId>(0)?, r.get::<_, RoleId>(1)?)))? {
        let (u, r) = row?;
        roles.entry(u).or_default().push(r);
    }
    for u in &mut users {
        u.roles = roles.remove(&u.id).unwrap_or_default();
    }
    Ok(users)
}

pub fn update_profile(conn: &Connection, id: UserId, profile: &Profile) -> DbResult<()> {
    conn.execute("UPDATE users SET profile = ?1 WHERE id = ?2", params![serde_json::to_string(profile).unwrap(), id])?;
    Ok(())
}

pub fn create_session(conn: &Connection, token: &str, user_id: UserId) -> DbResult<()> {
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at) VALUES (?1, ?2, ?3)",
        params![token, user_id, now_ms()],
    )?;
    Ok(())
}

pub fn session_user(conn: &Connection, token: &str) -> DbResult<Option<UserId>> {
    conn.query_row("SELECT user_id FROM sessions WHERE token = ?1", params![token], |r| r.get(0)).optional()
}

pub fn delete_session(conn: &Connection, token: &str) -> DbResult<()> {
    conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
    Ok(())
}

fn row_to_channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: r.get(0)?,
        name: r.get(1)?,
        topic: r.get(2)?,
        position: r.get(3)?,
        kind: kind_from_i64(r.get(4)?),
        last_message_id: r.get(5)?,
    })
}

/// Text and voice channels (not direct messages).
pub fn all_channels(conn: &Connection) -> DbResult<Vec<Channel>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, topic, position, kind,
                (SELECT MAX(m.id) FROM messages m WHERE m.channel_id = channels.id)
         FROM channels WHERE kind IN (0, 1) ORDER BY position, id",
    )?;
    stmt.query_map([], row_to_channel)?.collect()
}

/// The channel's kind, or `None` if it doesn't exist.
pub fn channel_kind(conn: &Connection, id: ChannelId) -> DbResult<Option<ChannelKind>> {
    conn.query_row("SELECT kind FROM channels WHERE id = ?1", params![id], |r| r.get(0).map(kind_from_i64)).optional()
}

/// The channel's kind if `user` may see it: everyone sees server channels,
/// only the two members see a direct conversation.
pub fn accessible_kind(conn: &Connection, user: UserId, id: ChannelId) -> DbResult<Option<ChannelKind>> {
    match channel_kind(conn, id)? {
        Some(ChannelKind::Direct) => Ok(is_direct_member(conn, id, user)?.then_some(ChannelKind::Direct)),
        other => Ok(other),
    }
}

pub fn create_channel(conn: &Connection, name: &str, topic: &str, kind: ChannelKind) -> DbResult<Channel> {
    let position: i64 = conn.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM channels", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO channels (name, topic, position, kind, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, topic, position, kind_to_i64(kind), now_ms()],
    )?;
    Ok(Channel {
        id: conn.last_insert_rowid(),
        name: name.to_owned(),
        topic: topic.to_owned(),
        position,
        kind,
        last_message_id: None,
    })
}

pub fn text_channel_count(conn: &Connection) -> DbResult<i64> {
    conn.query_row("SELECT COUNT(*) FROM channels WHERE kind = 0", [], |r| r.get(0))
}

/// Deletes the channel; returns ids of attachment files to remove from disk.
pub fn delete_channel(conn: &Connection, id: ChannelId) -> DbResult<Option<Vec<String>>> {
    let files = string_column(conn, "SELECT id FROM attachments WHERE channel_id = ?1", id)?;
    let deleted = conn.execute("DELETE FROM channels WHERE id = ?1", params![id])? > 0;
    Ok(deleted.then_some(files))
}

fn string_column(conn: &Connection, sql: &str, arg: i64) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.query_map(params![arg], |r| r.get(0))?.collect()
}

// ---- Direct messages

pub fn is_direct_member(conn: &Connection, channel: ChannelId, user: UserId) -> DbResult<bool> {
    conn.query_row(
        "SELECT 1 FROM direct_members WHERE channel_id = ?1 AND user_id = ?2",
        params![channel, user],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

pub fn direct_members(conn: &Connection, channel: ChannelId) -> DbResult<Vec<UserId>> {
    let mut stmt = conn.prepare_cached("SELECT user_id FROM direct_members WHERE channel_id = ?1")?;
    stmt.query_map(params![channel], |r| r.get(0))?.collect()
}

/// Everyone's direct conversations with `user`, newest activity first.
pub fn directs_for(conn: &Connection, user: UserId) -> DbResult<Vec<DirectChannel>> {
    let mut stmt = conn.prepare_cached(
        "SELECT me.channel_id, other.user_id,
                (SELECT MAX(m.id) FROM messages m WHERE m.channel_id = me.channel_id) AS last
         FROM direct_members me
         JOIN direct_members other ON other.channel_id = me.channel_id AND other.user_id != me.user_id
         WHERE me.user_id = ?1
         ORDER BY COALESCE(last, 0) DESC",
    )?;
    stmt.query_map(params![user], |r| {
        Ok(DirectChannel { id: r.get(0)?, user_id: r.get(1)?, last_message_id: r.get(2)? })
    })?
    .collect()
}

/// Find the conversation between `a` and `b`, creating it if needed.
/// Returns the channel id and whether it was just created.
pub fn open_direct(conn: &Connection, a: UserId, b: UserId) -> DbResult<(ChannelId, bool)> {
    let existing: Option<ChannelId> = conn
        .query_row(
            "SELECT x.channel_id FROM direct_members x
             JOIN direct_members y ON y.channel_id = x.channel_id
             WHERE x.user_id = ?1 AND y.user_id = ?2",
            params![a, b],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    let tx = conn.unchecked_transaction()?;
    let channel = create_channel(&tx, "", "", ChannelKind::Direct)?;
    tx.execute(
        "INSERT INTO direct_members (channel_id, user_id) VALUES (?1, ?2), (?1, ?3)",
        params![channel.id, a, b],
    )?;
    tx.commit()?;
    Ok((channel.id, true))
}

pub fn last_message_id(conn: &Connection, channel: ChannelId) -> DbResult<Option<MessageId>> {
    conn.query_row("SELECT MAX(id) FROM messages WHERE channel_id = ?1", params![channel], |r| r.get(0))
}

// ---- Read states

pub fn read_states(conn: &Connection, user: UserId) -> DbResult<Vec<ReadState>> {
    let mut stmt = conn.prepare_cached("SELECT channel_id, last_read FROM read_states WHERE user_id = ?1")?;
    stmt.query_map(params![user], |r| Ok(ReadState { channel_id: r.get(0)?, last_read: r.get(1)? }))?.collect()
}

/// Remember `user` has read `channel` up to `message` (never moves backwards).
pub fn mark_read(conn: &Connection, user: UserId, channel: ChannelId, message: MessageId) -> DbResult<()> {
    conn.execute(
        "INSERT INTO read_states (user_id, channel_id, last_read) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id, channel_id) DO UPDATE SET last_read = MAX(last_read, excluded.last_read)",
        params![user, channel, message],
    )?;
    Ok(())
}

// ---- Attachments

/// An uploaded file plus who may see it.
pub struct StoredAttachment {
    pub attachment: Attachment,
    pub uploader: UserId,
    pub channel_id: ChannelId,
    pub message_id: Option<MessageId>,
    pub has_thumb: bool,
}

pub fn insert_attachment(
    conn: &Connection,
    a: &Attachment,
    uploader: UserId,
    channel: ChannelId,
    has_thumb: bool,
) -> DbResult<()> {
    conn.execute(
        "INSERT INTO attachments
         (id, uploader_id, channel_id, filename, mime, size, width, height, animated, has_thumb, created_at, video)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            a.id,
            uploader,
            channel,
            a.filename,
            a.mime,
            a.size as i64,
            a.width,
            a.height,
            a.animated,
            has_thumb,
            now_ms(),
            a.video.map(|v| serde_json::to_string(&v).unwrap())
        ],
    )?;
    Ok(())
}

/// Store a link preview's picture as a file belonging to the message (so it's
/// downloadable by the same people and removed with it), but not listed as an attachment.
pub fn insert_embed_image(
    conn: &Connection,
    a: &Attachment,
    uploader: UserId,
    channel: ChannelId,
    message: MessageId,
    has_thumb: bool,
) -> DbResult<()> {
    insert_attachment(conn, a, uploader, channel, has_thumb)?;
    conn.execute("UPDATE attachments SET message_id = ?1, is_embed = 1 WHERE id = ?2", params![message, a.id])?;
    Ok(())
}

const ATTACHMENT_COLUMNS: &str =
    "id, filename, size, mime, width, height, animated, uploader_id, channel_id, message_id, has_thumb, video";

fn row_to_stored(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredAttachment> {
    Ok(StoredAttachment {
        attachment: Attachment {
            id: r.get(0)?,
            filename: r.get(1)?,
            size: r.get::<_, i64>(2)? as u64,
            mime: r.get(3)?,
            width: r.get(4)?,
            height: r.get(5)?,
            animated: r.get(6)?,
            video: r.get::<_, Option<String>>(11)?.and_then(|v| serde_json::from_str(&v).ok()),
        },
        uploader: r.get(7)?,
        channel_id: r.get(8)?,
        message_id: r.get(9)?,
        has_thumb: r.get(10)?,
    })
}

pub fn get_attachment(conn: &Connection, id: &str) -> DbResult<Option<StoredAttachment>> {
    conn.query_row(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachments WHERE id = ?1"), params![id], row_to_stored)
        .optional()
}

/// Uploads that were never attached to a message, older than `cutoff_ms`.
/// Deletes their rows and returns their ids so the files can be removed.
pub fn take_stale_uploads(conn: &Connection, cutoff_ms: i64) -> DbResult<Vec<String>> {
    let ids =
        string_column(conn, "SELECT id FROM attachments WHERE message_id IS NULL AND created_at < ?1", cutoff_ms)?;
    conn.execute("DELETE FROM attachments WHERE message_id IS NULL AND created_at < ?1", params![cutoff_ms])?;
    Ok(ids)
}

// ---- Reactions

/// Adds a reaction. Returns false if it was already there or the message has too many kinds.
pub fn add_reaction(
    conn: &Connection,
    message: MessageId,
    user: UserId,
    emoji: &str,
    max_kinds: usize,
) -> DbResult<bool> {
    let kinds: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT emoji) FROM reactions WHERE message_id = ?1 AND emoji != ?2",
        params![message, emoji],
        |r| r.get(0),
    )?;
    if kinds as usize >= max_kinds {
        return Ok(false);
    }
    let added = conn.execute(
        "INSERT OR IGNORE INTO reactions (message_id, user_id, emoji, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![message, user, emoji, now_ms()],
    )?;
    Ok(added > 0)
}

pub fn remove_reaction(conn: &Connection, message: MessageId, user: UserId, emoji: &str) -> DbResult<bool> {
    Ok(conn.execute(
        "DELETE FROM reactions WHERE message_id = ?1 AND user_id = ?2 AND emoji = ?3",
        params![message, user, emoji],
    )? > 0)
}

// ---- Messages

/// Fill in attachments and reactions for a batch of messages.
fn fill_extras(conn: &Connection, msgs: &mut [Message]) -> DbResult<()> {
    if msgs.is_empty() {
        return Ok(());
    }
    let (lo, hi) = (msgs.iter().map(|m| m.id).min().unwrap(), msgs.iter().map(|m| m.id).max().unwrap());
    let channel = msgs[0].channel_id;

    let mut by_message: HashMap<MessageId, Vec<Attachment>> = HashMap::new();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ATTACHMENT_COLUMNS} FROM attachments
         WHERE channel_id = ?1 AND message_id BETWEEN ?2 AND ?3 AND is_embed = 0 ORDER BY rowid"
    ))?;
    for row in stmt.query_map(params![channel, lo, hi], row_to_stored)? {
        let stored = row?;
        if let Some(mid) = stored.message_id {
            by_message.entry(mid).or_default().push(stored.attachment);
        }
    }

    let mut reactions: HashMap<MessageId, Vec<Reaction>> = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT r.message_id, r.emoji, r.user_id FROM reactions r
         JOIN messages m ON m.id = r.message_id
         WHERE m.channel_id = ?1 AND r.message_id BETWEEN ?2 AND ?3
         ORDER BY r.created_at, r.rowid",
    )?;
    for row in stmt.query_map(params![channel, lo, hi], |r| {
        Ok((r.get::<_, MessageId>(0)?, r.get::<_, String>(1)?, r.get::<_, UserId>(2)?))
    })? {
        let (mid, emoji, user) = row?;
        let list = reactions.entry(mid).or_default();
        match list.iter_mut().find(|x| x.emoji == emoji) {
            Some(x) => x.users.push(user),
            None => list.push(Reaction { emoji, users: vec![user] }),
        }
    }

    let mut mentions: HashMap<MessageId, Vec<UserId>> = HashMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT message_id, user_id FROM mentions WHERE channel_id = ?1 AND message_id BETWEEN ?2 AND ?3",
    )?;
    for row in stmt.query_map(params![channel, lo, hi], |r| Ok((r.get::<_, MessageId>(0)?, r.get::<_, UserId>(1)?)))? {
        let (mid, user) = row?;
        mentions.entry(mid).or_default().push(user);
    }

    for m in msgs.iter_mut() {
        m.attachments = by_message.remove(&m.id).unwrap_or_default();
        m.reactions = reactions.remove(&m.id).unwrap_or_default();
        m.mentions = mentions.remove(&m.id).unwrap_or_default();
    }
    Ok(())
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        channel_id: r.get(1)?,
        author_id: r.get(2)?,
        content: r.get(3)?,
        created_at: r.get(4)?,
        edited_at: r.get(5)?,
        attachments: Vec::new(),
        reactions: Vec::new(),
        embeds: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        mentions: Vec::new(),
        kind: r.get::<_, Option<String>>(7)?.and_then(|k| serde_json::from_str(&k).ok()).unwrap_or_default(),
    })
}

/// Change what kind of message this is (calls: who joined, when it ended).
pub fn set_kind(conn: &Connection, id: MessageId, kind: &MessageKind) -> DbResult<()> {
    let json = (!kind.is_normal()).then(|| serde_json::to_string(kind).unwrap());
    conn.execute("UPDATE messages SET kind = ?1 WHERE id = ?2", params![json, id])?;
    Ok(())
}

/// Calls still marked as going on (after a restart nobody is in them any more): end them now.
pub fn end_stale_calls(conn: &Connection) -> DbResult<Vec<MessageId>> {
    let mut stmt = conn.prepare("SELECT id, kind FROM messages WHERE kind LIKE '%\"ended_at\":null%'")?;
    let rows: Vec<(MessageId, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<DbResult<_>>()?;
    let mut ended = Vec::new();
    for (id, json) in rows {
        if let Ok(MessageKind::Call { participants, ended_at: None }) = serde_json::from_str(&json) {
            set_kind(conn, id, &MessageKind::Call { participants, ended_at: Some(now_ms()) })?;
            ended.push(id);
        }
    }
    Ok(ended)
}

/// Why a message with attachments couldn't be sent.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    BadAttachment,
}

/// Store a message and attach previously uploaded files to it, all or nothing.
pub fn insert_message(
    conn: &Connection,
    channel_id: ChannelId,
    author_id: UserId,
    content: &str,
    attachment_ids: &[String],
) -> DbResult<Result<Message, SendError>> {
    let tx = conn.unchecked_transaction()?;
    let created_at = now_ms();
    tx.execute(
        "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![channel_id, author_id, content, created_at],
    )?;
    let id = tx.last_insert_rowid();
    let mut attachments = Vec::new();
    for aid in attachment_ids {
        let claimed = tx.execute(
            "UPDATE attachments SET message_id = ?1
             WHERE id = ?2 AND uploader_id = ?3 AND channel_id = ?4 AND message_id IS NULL",
            params![id, aid, author_id, channel_id],
        )?;
        if claimed != 1 {
            return Ok(Err(SendError::BadAttachment)); // dropping `tx` rolls back
        }
        if let Some(a) = get_attachment(&tx, aid)? {
            attachments.push(a.attachment);
        }
    }
    tx.commit()?;
    Ok(Ok(Message {
        id,
        channel_id,
        author_id,
        content: content.to_owned(),
        created_at,
        edited_at: None,
        attachments,
        reactions: Vec::new(),
        embeds: Vec::new(),
        mentions: Vec::new(),
        kind: MessageKind::Normal,
    }))
}

pub fn get_message(conn: &Connection, id: MessageId) -> DbResult<Option<Message>> {
    let msg = conn
        .query_row(
            "SELECT id, channel_id, author_id, content, created_at, edited_at, embeds, kind FROM messages WHERE id = ?1",
            params![id],
            row_to_message,
        )
        .optional()?;
    Ok(match msg {
        Some(mut m) => {
            fill_extras(conn, std::slice::from_mut(&mut m))?;
            Some(m)
        }
        None => None,
    })
}

pub fn edit_message(conn: &Connection, id: MessageId, content: &str) -> DbResult<()> {
    conn.execute("UPDATE messages SET content = ?1, edited_at = ?2 WHERE id = ?3", params![content, now_ms(), id])?;
    Ok(())
}

/// Deletes the message; returns ids of attachment files to remove from disk.
pub fn delete_message(conn: &Connection, id: MessageId) -> DbResult<Vec<String>> {
    let files = string_column(conn, "SELECT id FROM attachments WHERE message_id = ?1", id)?;
    conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
    Ok(files)
}

/// Returns up to [`HISTORY_PAGE`] messages older than `before`, oldest first.
pub fn history(conn: &Connection, channel_id: ChannelId, before: Option<MessageId>) -> DbResult<Vec<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, channel_id, author_id, content, created_at, edited_at, embeds, kind FROM messages
         WHERE channel_id = ?1 AND (?2 IS NULL OR id < ?2)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let mut msgs: Vec<Message> =
        stmt.query_map(params![channel_id, before, HISTORY_PAGE as i64], row_to_message)?.collect::<DbResult<_>>()?;
    msgs.reverse();
    fill_extras(conn, &mut msgs)?;
    Ok(msgs)
}

// ---- Search

/// Fill in attachments and the rest for messages from several channels.
fn fill_mixed(conn: &Connection, msgs: &mut [Message]) -> DbResult<()> {
    let mut channels: Vec<ChannelId> = msgs.iter().map(|m| m.channel_id).collect();
    channels.sort();
    channels.dedup();
    for ch in channels {
        let mut group: Vec<Message> = msgs.iter().filter(|m| m.channel_id == ch).cloned().collect();
        fill_extras(conn, &mut group)?;
        for g in group {
            if let Some(slot) = msgs.iter_mut().find(|m| m.id == g.id) {
                *slot = g;
            }
        }
    }
    Ok(())
}

/// Messages `user` can see that match, newest first (one page, plus whether there are more).
pub fn search(conn: &Connection, user: UserId, q: &SearchQuery) -> DbResult<(Vec<Message>, bool)> {
    let mut sql = String::from(
        "SELECT id, channel_id, author_id, content, created_at, edited_at, embeds, kind FROM messages m
         WHERE (m.channel_id IN (SELECT id FROM channels WHERE kind = 0)
                OR m.channel_id IN (SELECT channel_id FROM direct_members WHERE user_id = ?))",
    );
    let mut args: Vec<rusqlite::types::Value> = vec![user.into()];
    for w in q.words.iter().filter(|w| !w.trim().is_empty()).take(8) {
        // LIKE wildcards in the search are matched literally.
        let escaped = w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        sql.push_str(" AND m.content LIKE ? ESCAPE '\\'");
        args.push(format!("%{escaped}%").into());
    }
    if let Some(from) = q.from {
        sql.push_str(" AND m.author_id = ?");
        args.push(from.into());
    }
    if let Some(ch) = q.channel_id {
        sql.push_str(" AND m.channel_id = ?");
        args.push(ch.into());
    }
    if let Some(before) = q.before {
        sql.push_str(" AND m.id < ?");
        args.push(before.into());
    }
    match q.has {
        Some(SearchHas::Link) => sql.push_str(" AND (m.content LIKE '%http://%' OR m.content LIKE '%https://%')"),
        Some(SearchHas::File) => {
            sql.push_str(" AND EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id AND a.is_embed = 0)")
        }
        Some(SearchHas::Image) => sql.push_str(
            " AND EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id AND a.is_embed = 0
              AND a.width IS NOT NULL AND a.video IS NULL)",
        ),
        Some(SearchHas::Video) => {
            sql.push_str(" AND EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id AND a.video IS NOT NULL)")
        }
        None => {}
    }
    sql.push_str(&format!(" ORDER BY m.id DESC LIMIT {}", SEARCH_PAGE + 1));
    let mut stmt = conn.prepare(&sql)?;
    let mut msgs: Vec<Message> =
        stmt.query_map(rusqlite::params_from_iter(args), row_to_message)?.collect::<DbResult<_>>()?;
    let more = msgs.len() > SEARCH_PAGE;
    msgs.truncate(SEARCH_PAGE);
    fill_mixed(conn, &mut msgs)?;
    Ok((msgs, more))
}

/// Up to 40 messages before `id` and 5 after it (so it shows near the bottom), oldest first.
pub fn around(conn: &Connection, channel_id: ChannelId, id: MessageId) -> DbResult<(Vec<Message>, bool)> {
    let cols = "id, channel_id, author_id, content, created_at, edited_at, embeds, kind";
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {cols} FROM messages WHERE channel_id = ?1 AND id <= ?2 ORDER BY id DESC LIMIT 41"
    ))?;
    let mut older: Vec<Message> = stmt.query_map(params![channel_id, id], row_to_message)?.collect::<DbResult<_>>()?;
    let reached_start = older.len() < 41;
    older.truncate(40);
    older.reverse();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {cols} FROM messages WHERE channel_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT 5"
    ))?;
    let newer: Vec<Message> = stmt.query_map(params![channel_id, id], row_to_message)?.collect::<DbResult<_>>()?;
    older.extend(newer);
    fill_extras(conn, &mut older)?;
    Ok((older, reached_start))
}

// ---- Roles and permissions

fn row_to_role(r: &rusqlite::Row<'_>) -> rusqlite::Result<Role> {
    Ok(Role {
        id: r.get(0)?,
        name: r.get(1)?,
        color: r.get::<_, Option<i64>>(2)?.map(|c| c as u32),
        permissions: r.get::<_, i64>(3)? as u64,
        position: r.get(4)?,
        hoist: r.get(5)?,
    })
}

const ROLE_COLUMNS: &str = "id, name, color, permissions, position, hoist";

/// All roles, highest first.
pub fn all_roles(conn: &Connection) -> DbResult<Vec<Role>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {ROLE_COLUMNS} FROM roles ORDER BY position DESC, id"))?;
    stmt.query_map([], row_to_role)?.collect()
}

pub fn get_role(conn: &Connection, id: RoleId) -> DbResult<Option<Role>> {
    conn.query_row(&format!("SELECT {ROLE_COLUMNS} FROM roles WHERE id = ?1"), params![id], row_to_role).optional()
}

pub fn role_count(conn: &Connection) -> DbResult<usize> {
    conn.query_row("SELECT COUNT(*) FROM roles", [], |r| r.get::<_, i64>(0)).map(|n| n as usize)
}

/// A new role goes just above @everyone (the lowest place), pushing the others up.
pub fn create_role(conn: &Connection, name: &str) -> DbResult<Role> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE roles SET position = position + 1 WHERE id != ?1", params![EVERYONE_ROLE])?;
    tx.execute("INSERT INTO roles (name, color, permissions, position) VALUES (?1, NULL, 0, 1)", params![name])?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(get_role(conn, id)?.expect("just inserted"))
}

pub fn update_role(conn: &Connection, role: &Role) -> DbResult<()> {
    conn.execute(
        "UPDATE roles SET name = ?1, color = ?2, permissions = ?3, position = ?4, hoist = ?5 WHERE id = ?6",
        params![role.name, role.color.map(|c| c as i64), role.permissions as i64, role.position, role.hoist, role.id],
    )?;
    Ok(())
}

/// Give roles positions 1, 2, 3... in their current order (after a move), keeping @everyone at 0.
pub fn renumber_roles(conn: &Connection) -> DbResult<Vec<Role>> {
    let mut roles = all_roles(conn)?;
    roles.retain(|r| r.id != EVERYONE_ROLE);
    roles.reverse(); // lowest first
    for (i, r) in roles.iter_mut().enumerate() {
        r.position = i as i64 + 1;
        conn.execute("UPDATE roles SET position = ?1 WHERE id = ?2", params![r.position, r.id])?;
    }
    all_roles(conn)
}

pub fn delete_role(conn: &Connection, id: RoleId) -> DbResult<()> {
    conn.execute("DELETE FROM roles WHERE id = ?1 AND id != ?2", params![id, EVERYONE_ROLE])?;
    Ok(())
}

pub fn user_roles(conn: &Connection, user: UserId) -> DbResult<Vec<RoleId>> {
    let mut stmt = conn.prepare_cached("SELECT role_id FROM user_roles WHERE user_id = ?1 ORDER BY role_id")?;
    stmt.query_map(params![user], |r| r.get(0))?.collect()
}

pub fn set_user_roles(conn: &Connection, user: UserId, roles: &[RoleId]) -> DbResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM user_roles WHERE user_id = ?1", params![user])?;
    for r in roles {
        if *r != EVERYONE_ROLE {
            tx.execute("INSERT OR IGNORE INTO user_roles (user_id, role_id) VALUES (?1, ?2)", params![user, r])?;
        }
    }
    tx.commit()
}

/// What someone may do, and the position of their highest role. The owner may do everything.
pub fn access(conn: &Connection, user: UserId) -> DbResult<Access> {
    let is_owner: bool = conn
        .query_row("SELECT is_owner FROM users WHERE id = ?1", params![user], |r| r.get(0))
        .optional()?
        .unwrap_or(false);
    let mut stmt = conn.prepare_cached(
        "SELECT permissions, position FROM roles
         WHERE id = ?1 OR id IN (SELECT role_id FROM user_roles WHERE user_id = ?2)",
    )?;
    let mut permissions = 0u64;
    let mut top = 0i64;
    for row in stmt.query_map(params![EVERYONE_ROLE, user], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
        let (p, pos) = row?;
        permissions |= p as u64;
        top = top.max(pos);
    }
    if is_owner || permissions & perms::ADMINISTRATOR != 0 {
        permissions = perms::ALL;
    }
    Ok(Access { is_owner, permissions, top })
}

#[derive(Debug, Clone, Copy)]
pub struct Access {
    pub is_owner: bool,
    pub permissions: u64,
    /// Position of their highest role: they can only manage roles below it.
    pub top: i64,
}

impl Access {
    pub fn can(&self, permission: u64) -> bool {
        self.permissions & permission == permission
    }

    /// Whether they may edit, delete or hand out this role.
    pub fn can_manage_role(&self, role: &Role) -> bool {
        role.id != EVERYONE_ROLE && self.can(perms::MANAGE_ROLES) && (self.is_owner || role.position < self.top)
    }
}

// ---- Blocking

pub fn set_blocked(conn: &Connection, user: UserId, other: UserId, blocked: bool) -> DbResult<()> {
    if blocked {
        conn.execute("INSERT OR IGNORE INTO blocks (user_id, blocked_id) VALUES (?1, ?2)", params![user, other])?;
    } else {
        conn.execute("DELETE FROM blocks WHERE user_id = ?1 AND blocked_id = ?2", params![user, other])?;
    }
    Ok(())
}

pub fn blocked_by(conn: &Connection, user: UserId) -> DbResult<Vec<UserId>> {
    let mut stmt = conn.prepare_cached("SELECT blocked_id FROM blocks WHERE user_id = ?1")?;
    stmt.query_map(params![user], |r| r.get(0))?.collect()
}

/// Whether either of the two has blocked the other.
pub fn blocked_between(conn: &Connection, a: UserId, b: UserId) -> DbResult<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM blocks WHERE (user_id = ?1 AND blocked_id = ?2) OR (user_id = ?2 AND blocked_id = ?1))",
        params![a, b],
        |r| r.get(0),
    )
}

// ---- Mentions

/// Who a message pings: mentioned people, holders of mentioned roles, and everyone
/// for @everyone (only if the author may). Never the author, nor people who blocked them.
pub fn resolve_mentions(conn: &Connection, author: UserId, channel: ChannelId, content: &str) -> DbResult<Vec<UserId>> {
    let (users, roles, everyone) = parse_mentions(content);
    let mut out: Vec<UserId> = Vec::new();
    let members: Option<Vec<UserId>> = match channel_kind(conn, channel)? {
        Some(ChannelKind::Direct) => Some(direct_members(conn, channel)?),
        _ => None,
    };
    let everyone = everyone && (members.is_some() || access(conn, author)?.can(perms::MENTION_EVERYONE));
    let all: Vec<UserId> = match &members {
        Some(m) => m.clone(),
        None => all_users(conn)?.into_iter().map(|u| u.id).collect(),
    };
    for id in all {
        let roles_of = if roles.is_empty() { Vec::new() } else { user_roles(conn, id)? };
        if everyone || users.contains(&id) || roles_of.iter().any(|r| roles.contains(r)) {
            out.push(id);
        }
    }
    let blockers = {
        let mut stmt = conn.prepare_cached("SELECT user_id FROM blocks WHERE blocked_id = ?1")?;
        stmt.query_map(params![author], |r| r.get::<_, UserId>(0))?.collect::<DbResult<Vec<_>>>()?
    };
    out.retain(|u| *u != author && !blockers.contains(u));
    Ok(out)
}

pub fn set_mentions(conn: &Connection, message: MessageId, channel: ChannelId, users: &[UserId]) -> DbResult<()> {
    conn.execute("DELETE FROM mentions WHERE message_id = ?1", params![message])?;
    for u in users {
        conn.execute(
            "INSERT OR IGNORE INTO mentions (message_id, user_id, channel_id) VALUES (?1, ?2, ?3)",
            params![message, u, channel],
        )?;
    }
    Ok(())
}

/// Unread mentions of someone, per channel.
pub fn mention_counts(conn: &Connection, user: UserId) -> DbResult<Vec<MentionCount>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.channel_id, COUNT(*) FROM mentions m
         LEFT JOIN read_states r ON r.user_id = m.user_id AND r.channel_id = m.channel_id
         WHERE m.user_id = ?1 AND m.message_id > COALESCE(r.last_read, 0)
         GROUP BY m.channel_id",
    )?;
    stmt.query_map(params![user], |r| Ok(MentionCount { channel_id: r.get(0)?, count: r.get::<_, i64>(1)? as u32 }))?
        .collect()
}

// ---- Link previews

pub fn set_embeds(conn: &Connection, message: MessageId, embeds: &[Embed]) -> DbResult<()> {
    conn.execute(
        "UPDATE messages SET embeds = ?1 WHERE id = ?2",
        params![serde_json::to_string(embeds).unwrap(), message],
    )?;
    Ok(())
}

/// Files of a message's link previews (to delete when the previews are replaced).
pub fn take_embed_images(conn: &Connection, message: MessageId) -> DbResult<Vec<String>> {
    let ids = string_column(conn, "SELECT id FROM attachments WHERE message_id = ?1 AND is_embed = 1", message)?;
    conn.execute("DELETE FROM attachments WHERE message_id = ?1 AND is_embed = 1", params![message])?;
    Ok(ids)
}

// ---- Profile pictures and banners

pub fn insert_profile_image(conn: &Connection, id: &str, user: UserId, mime: &str, animated: bool) -> DbResult<()> {
    conn.execute(
        "INSERT INTO profile_images (id, user_id, mime, animated, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, user, mime, animated, now_ms()],
    )?;
    Ok(())
}

pub fn profile_image_mime(conn: &Connection, id: &str) -> DbResult<Option<String>> {
    conn.query_row("SELECT mime FROM profile_images WHERE id = ?1", params![id], |r| r.get(0)).optional()
}

pub fn delete_profile_image(conn: &Connection, id: &str) -> DbResult<()> {
    conn.execute("DELETE FROM profile_images WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_pages() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .call(|c| {
                let u = create_user(c, "alice", "x", true)?;
                let ch = all_channels(c)?.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
                let mut ids = vec![];
                for i in 0..(HISTORY_PAGE + 5) {
                    ids.push(insert_message(c, ch, u.id, &format!("m{i}"), &[])?.unwrap().id);
                }
                Ok((ch, ids))
            })
            .await
            .unwrap();
        let (ch, ids) = ids;
        let page = db.call(move |c| history(c, ch, None)).await.unwrap();
        assert_eq!(page.len(), HISTORY_PAGE);
        assert_eq!(page.last().unwrap().id, *ids.last().unwrap());
        let first = page[0].id;
        let older = db.call(move |c| history(c, ch, Some(first))).await.unwrap();
        assert_eq!(older.len(), 5);
        assert_eq!(older[0].content, "m0");
    }

    #[test]
    fn migrates_old_channels_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT NOT NULL, topic TEXT NOT NULL DEFAULT '',
             position INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL);
             INSERT INTO channels (name, created_at) VALUES ('old', 0);",
        )
        .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent
        let chans = all_channels(&conn).unwrap();
        assert_eq!(chans[0].kind, ChannelKind::Text);
    }

    fn att(id: &str) -> Attachment {
        Attachment {
            id: id.into(),
            filename: "cat.png".into(),
            size: 10,
            mime: "image/png".into(),
            width: Some(4),
            height: Some(3),
            animated: false,
            video: None,
        }
    }

    #[test]
    fn directs_reactions_attachments_and_read_states() {
        let db = Db::open_in_memory().unwrap();
        let c = db.lock();
        let a = create_user(&c, "alice", "x", true).unwrap().id;
        let b = create_user(&c, "bob", "x", false).unwrap().id;
        let e = create_user(&c, "eve", "x", false).unwrap().id;

        // DMs: created once, private to the two members, hidden from the channel list.
        let (dm, created) = open_direct(&c, a, b).unwrap();
        assert!(created);
        assert_eq!(open_direct(&c, b, a).unwrap(), (dm, false));
        assert_eq!(accessible_kind(&c, a, dm).unwrap(), Some(ChannelKind::Direct));
        assert_eq!(accessible_kind(&c, e, dm).unwrap(), None);
        assert!(all_channels(&c).unwrap().iter().all(|ch| ch.id != dm));
        assert_eq!(directs_for(&c, a).unwrap()[0].user_id, b);
        assert!(directs_for(&c, e).unwrap().is_empty());

        // Attachments: only the uploader can attach, only once, only in the right channel.
        insert_attachment(&c, &att("f1"), a, dm, false).unwrap();
        assert_eq!(insert_message(&c, dm, b, "", &["f1".into()]).unwrap(), Err(SendError::BadAttachment));
        let general = all_channels(&c).unwrap()[0].id;
        assert_eq!(insert_message(&c, general, a, "", &["f1".into()]).unwrap(), Err(SendError::BadAttachment));
        let msg = insert_message(&c, dm, a, "look", &["f1".into()]).unwrap().unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(insert_message(&c, dm, a, "again", &["f1".into()]).unwrap(), Err(SendError::BadAttachment));
        // A failed send doesn't leave a half-written message behind.
        assert_eq!(history(&c, dm, None).unwrap().len(), 1);

        // Reactions are grouped per emoji and counted once per person.
        assert!(add_reaction(&c, msg.id, a, "👍", 20).unwrap());
        assert!(!add_reaction(&c, msg.id, a, "👍", 20).unwrap());
        assert!(add_reaction(&c, msg.id, b, "👍", 20).unwrap());
        assert!(add_reaction(&c, msg.id, b, "🎉", 20).unwrap());
        assert!(!add_reaction(&c, msg.id, b, "😂", 2).unwrap(), "limit on different emoji");
        let m = get_message(&c, msg.id).unwrap().unwrap();
        assert_eq!(m.reactions[0], Reaction { emoji: "👍".into(), users: vec![a, b] });
        assert_eq!(m.reactions.len(), 2);
        assert!(remove_reaction(&c, msg.id, a, "👍").unwrap());
        assert_eq!(history(&c, dm, None).unwrap()[0].reactions[0].users, vec![b]);

        // Read states only move forward.
        mark_read(&c, b, dm, msg.id).unwrap();
        mark_read(&c, b, dm, msg.id - 1).unwrap();
        assert_eq!(read_states(&c, b).unwrap(), vec![ReadState { channel_id: dm, last_read: msg.id }]);

        // Deleting returns the files to clean up.
        assert_eq!(delete_message(&c, msg.id).unwrap(), vec!["f1".to_string()]);
        assert!(get_attachment(&c, "f1").unwrap().is_none());

        // Unused uploads expire.
        insert_attachment(&c, &att("old"), a, general, false).unwrap();
        assert_eq!(take_stale_uploads(&c, now_ms() + 1).unwrap(), vec!["old".to_string()]);
    }

    #[tokio::test]
    async fn usernames_case_insensitive() {
        let db = Db::open_in_memory().unwrap();
        db.call(|c| create_user(c, "Bob", "x", false)).await.unwrap();
        assert!(db.call(|c| create_user(c, "bob", "y", false)).await.is_err());
        assert!(db.call(|c| credentials(c, "BOB")).await.unwrap().is_some());
    }
}
