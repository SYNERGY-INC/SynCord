//! SQLite storage. A single connection behind a mutex is plenty for a friend
//! group and keeps memory use tiny; all calls run on tokio's blocking pool.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use syncord_proto::{Channel, ChannelId, HISTORY_PAGE, Message, MessageId, Profile, User, UserId};

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
"#;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.seed()?;
        Ok(db)
    }

    /// A throwaway database, used by tests.
    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.seed()?;
        Ok(db)
    }

    fn seed(&self) -> DbResult<()> {
        let conn = self.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))?;
        if count == 0 {
            conn.execute(
                "INSERT INTO channels (name, topic, position, created_at) VALUES ('general', 'Say hi!', 0, ?1)",
                params![now_ms()],
            )?;
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
        tokio::task::spawn_blocking(move || f(&db.lock()))
            .await
            .expect("database task panicked")
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
    const COLORS: [u32; 8] =
        [0x5865F2, 0x3BA55C, 0xFAA61A, 0xED4245, 0xEB459E, 0x1ABC9C, 0x9B59B6, 0xE67E22];
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
    conn.execute(
        "UPDATE users SET profile = ?1 WHERE id = ?2",
        params![serde_json::to_string(profile).unwrap(), id],
    )?;
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
    Ok(Channel { id: r.get(0)?, name: r.get(1)?, topic: r.get(2)?, position: r.get(3)? })
}

pub fn all_channels(conn: &Connection) -> DbResult<Vec<Channel>> {
    let mut stmt = conn.prepare_cached("SELECT id, name, topic, position FROM channels ORDER BY position, id")?;
    stmt.query_map([], row_to_channel)?.collect()
}

pub fn channel_exists(conn: &Connection, id: ChannelId) -> DbResult<bool> {
    conn.query_row("SELECT 1 FROM channels WHERE id = ?1", params![id], |_| Ok(())).optional().map(|r| r.is_some())
}

pub fn create_channel(conn: &Connection, name: &str, topic: &str) -> DbResult<Channel> {
    let position: i64 =
        conn.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM channels", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO channels (name, topic, position, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![name, topic, position, now_ms()],
    )?;
    Ok(Channel { id: conn.last_insert_rowid(), name: name.to_owned(), topic: topic.to_owned(), position })
}

pub fn channel_count(conn: &Connection) -> DbResult<i64> {
    conn.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))
}

pub fn delete_channel(conn: &Connection, id: ChannelId) -> DbResult<bool> {
    Ok(conn.execute("DELETE FROM channels WHERE id = ?1", params![id])? > 0)
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        channel_id: r.get(1)?,
        author_id: r.get(2)?,
        content: r.get(3)?,
        created_at: r.get(4)?,
        edited_at: r.get(5)?,
    })
}

pub fn insert_message(conn: &Connection, channel_id: ChannelId, author_id: UserId, content: &str) -> DbResult<Message> {
    let created_at = now_ms();
    conn.execute(
        "INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![channel_id, author_id, content, created_at],
    )?;
    Ok(Message {
        id: conn.last_insert_rowid(),
        channel_id,
        author_id,
        content: content.to_owned(),
        created_at,
        edited_at: None,
    })
}

pub fn get_message(conn: &Connection, id: MessageId) -> DbResult<Option<Message>> {
    conn.query_row(
        "SELECT id, channel_id, author_id, content, created_at, edited_at FROM messages WHERE id = ?1",
        params![id],
        row_to_message,
    )
    .optional()
}

pub fn edit_message(conn: &Connection, id: MessageId, content: &str) -> DbResult<()> {
    conn.execute("UPDATE messages SET content = ?1, edited_at = ?2 WHERE id = ?3", params![content, now_ms(), id])?;
    Ok(())
}

pub fn delete_message(conn: &Connection, id: MessageId) -> DbResult<()> {
    conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
    Ok(())
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
                let ch = all_channels(c)?[0].id;
                let mut ids = vec![];
                for i in 0..(HISTORY_PAGE + 5) {
                    ids.push(insert_message(c, ch, u.id, &format!("m{i}"))?.id);
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

    #[tokio::test]
    async fn usernames_case_insensitive() {
        let db = Db::open_in_memory().unwrap();
        db.call(|c| create_user(c, "Bob", "x", false)).await.unwrap();
        assert!(db.call(|c| create_user(c, "bob", "y", false)).await.is_err());
        assert!(db.call(|c| credentials(c, "BOB")).await.unwrap().is_some());
    }
}
