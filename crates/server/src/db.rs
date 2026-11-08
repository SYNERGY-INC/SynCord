//! SQLite storage. A single connection behind a mutex is plenty for a friend
//! group and keeps memory use tiny; all calls run on tokio's blocking pool.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use syncord_proto::{
    Attachment, Channel, ChannelId, ChannelKind, DirectChannel, HISTORY_PAGE, Message, MessageId, Profile, Reaction,
    ReadState, User, UserId,
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
    Ok(User { id: conn.last_insert_rowid(), username: username.to_owned(), profile, is_owner })
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
    conn.query_row("SELECT id, username, profile, is_owner FROM users WHERE id = ?1", params![id], row_to_user)
        .optional()
}

pub fn all_users(conn: &Connection) -> DbResult<Vec<User>> {
    let mut stmt = conn.prepare_cached("SELECT id, username, profile, is_owner FROM users ORDER BY id")?;
    stmt.query_map([], row_to_user)?.collect()
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
         (id, uploader_id, channel_id, filename, mime, size, width, height, animated, has_thumb, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            now_ms()
        ],
    )?;
    Ok(())
}

const ATTACHMENT_COLUMNS: &str =
    "id, filename, size, mime, width, height, animated, uploader_id, channel_id, message_id, has_thumb";

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
         WHERE channel_id = ?1 AND message_id BETWEEN ?2 AND ?3 ORDER BY rowid"
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

    for m in msgs.iter_mut() {
        m.attachments = by_message.remove(&m.id).unwrap_or_default();
        m.reactions = reactions.remove(&m.id).unwrap_or_default();
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
    })
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
    }))
}

pub fn get_message(conn: &Connection, id: MessageId) -> DbResult<Option<Message>> {
    let msg = conn
        .query_row(
            "SELECT id, channel_id, author_id, content, created_at, edited_at FROM messages WHERE id = ?1",
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
        "SELECT id, channel_id, author_id, content, created_at, edited_at FROM messages
         WHERE channel_id = ?1 AND (?2 IS NULL OR id < ?2)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let mut msgs: Vec<Message> =
        stmt.query_map(params![channel_id, before, HISTORY_PAGE as i64], row_to_message)?.collect::<DbResult<_>>()?;
    msgs.reverse();
    fill_extras(conn, &mut msgs)?;
    Ok(msgs)
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
