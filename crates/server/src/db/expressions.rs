//! 0.7 storage: custom emoji, stickers and soundboard sounds. Their files live in the
//! uploads folder under random ids, like attachments.

use celerity_proto::{CustomEmoji, EmojiId, Sound, SoundId, Sticker, StickerId, UserId};
use rusqlite::{Connection, OptionalExtension, params};

use super::{DbResult, now_ms};

/// An emoji or sticker as stored: what everyone sees plus its file.
pub struct StoredPicture {
    pub id: i64,
    pub name: String,
    pub file: String,
    pub mime: String,
    pub animated: bool,
    pub created_by: UserId,
}

impl StoredPicture {
    pub fn emoji(&self) -> CustomEmoji {
        CustomEmoji { id: self.id, name: self.name.clone(), animated: self.animated, created_by: self.created_by }
    }

    pub fn sticker(&self) -> Sticker {
        Sticker {
            id: self.id,
            name: self.name.clone(),
            animated: self.animated,
            mime: self.mime.clone(),
            created_by: self.created_by,
        }
    }
}

/// Emoji and stickers are stored the same way, in these two tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureKind {
    Emoji,
    Sticker,
}

impl PictureKind {
    fn table(self) -> &'static str {
        match self {
            PictureKind::Emoji => "emojis",
            PictureKind::Sticker => "stickers",
        }
    }
}

fn row_to_picture(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPicture> {
    Ok(StoredPicture {
        id: r.get(0)?,
        name: r.get(1)?,
        file: r.get(2)?,
        mime: r.get(3)?,
        animated: r.get(4)?,
        created_by: r.get(5)?,
    })
}

pub fn all_pictures(conn: &Connection, kind: PictureKind) -> DbResult<Vec<StoredPicture>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, name, file, mime, animated, created_by FROM {} ORDER BY id",
        kind.table()
    ))?;
    stmt.query_map([], row_to_picture)?.collect()
}

pub fn get_picture(conn: &Connection, kind: PictureKind, id: i64) -> DbResult<Option<StoredPicture>> {
    conn.query_row(
        &format!("SELECT id, name, file, mime, animated, created_by FROM {} WHERE id = ?1", kind.table()),
        params![id],
        row_to_picture,
    )
    .optional()
}

pub fn picture_count(conn: &Connection, kind: PictureKind) -> DbResult<usize> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {}", kind.table()), [], |r| r.get::<_, i64>(0)).map(|n| n as usize)
}

pub fn picture_name_taken(conn: &Connection, kind: PictureKind, name: &str, except: i64) -> DbResult<bool> {
    conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM {} WHERE name = ?1 AND id != ?2)", kind.table()),
        params![name, except],
        |r| r.get(0),
    )
}

pub fn insert_picture(
    conn: &Connection,
    kind: PictureKind,
    name: &str,
    file: &str,
    mime: &str,
    animated: bool,
    by: UserId,
) -> DbResult<StoredPicture> {
    conn.execute(
        &format!(
            "INSERT INTO {} (name, file, mime, animated, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            kind.table()
        ),
        params![name, file, mime, animated, by, now_ms()],
    )?;
    let id = conn.last_insert_rowid();
    Ok(StoredPicture { id, name: name.into(), file: file.into(), mime: mime.into(), animated, created_by: by })
}

pub fn rename_picture(conn: &Connection, kind: PictureKind, id: i64, name: &str) -> DbResult<bool> {
    Ok(conn.execute(&format!("UPDATE {} SET name = ?1 WHERE id = ?2", kind.table()), params![name, id])? > 0)
}

/// Deletes it; returns its file to remove.
pub fn delete_picture(conn: &Connection, kind: PictureKind, id: i64) -> DbResult<Option<String>> {
    let file = get_picture(conn, kind, id)?.map(|p| p.file);
    conn.execute(&format!("DELETE FROM {} WHERE id = ?1", kind.table()), params![id])?;
    Ok(file)
}

pub fn all_emojis(conn: &Connection) -> DbResult<Vec<CustomEmoji>> {
    Ok(all_pictures(conn, PictureKind::Emoji)?.iter().map(StoredPicture::emoji).collect())
}

pub fn all_stickers(conn: &Connection) -> DbResult<Vec<Sticker>> {
    Ok(all_pictures(conn, PictureKind::Sticker)?.iter().map(StoredPicture::sticker).collect())
}

pub fn get_sticker(conn: &Connection, id: StickerId) -> DbResult<Option<Sticker>> {
    Ok(get_picture(conn, PictureKind::Sticker, id)?.map(|p| p.sticker()))
}

pub fn get_emoji(conn: &Connection, id: EmojiId) -> DbResult<Option<CustomEmoji>> {
    Ok(get_picture(conn, PictureKind::Emoji, id)?.map(|p| p.emoji()))
}

// ---- Sounds

/// A sound as stored, plus its file.
pub struct StoredSound {
    pub sound: Sound,
    pub file: String,
}

fn row_to_sound(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSound> {
    Ok(StoredSound {
        sound: Sound {
            id: r.get(0)?,
            name: r.get(1)?,
            emoji: r.get(2)?,
            mime: r.get(4)?,
            duration_ms: r.get(5)?,
            size: r.get::<_, i64>(6)? as u64,
            created_by: r.get(7)?,
        },
        file: r.get(3)?,
    })
}

const SOUND_COLUMNS: &str = "id, name, emoji, file, mime, duration_ms, size, created_by";

pub fn all_sounds(conn: &Connection) -> DbResult<Vec<Sound>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {SOUND_COLUMNS} FROM sounds ORDER BY id"))?;
    stmt.query_map([], |r| row_to_sound(r).map(|s| s.sound))?.collect()
}

pub fn get_sound(conn: &Connection, id: SoundId) -> DbResult<Option<StoredSound>> {
    conn.query_row(&format!("SELECT {SOUND_COLUMNS} FROM sounds WHERE id = ?1"), params![id], row_to_sound).optional()
}

pub fn sound_count(conn: &Connection) -> DbResult<usize> {
    conn.query_row("SELECT COUNT(*) FROM sounds", [], |r| r.get::<_, i64>(0)).map(|n| n as usize)
}

pub fn insert_sound(conn: &Connection, sound: &Sound, file: &str) -> DbResult<Sound> {
    conn.execute(
        "INSERT INTO sounds (name, emoji, file, mime, duration_ms, size, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            sound.name,
            sound.emoji,
            file,
            sound.mime,
            sound.duration_ms,
            sound.size as i64,
            sound.created_by,
            now_ms()
        ],
    )?;
    Ok(Sound { id: conn.last_insert_rowid(), ..sound.clone() })
}

pub fn update_sound(conn: &Connection, id: SoundId, name: &str, emoji: Option<&str>) -> DbResult<bool> {
    Ok(conn.execute("UPDATE sounds SET name = ?1, emoji = ?2 WHERE id = ?3", params![name, emoji, id])? > 0)
}

/// Deletes it; returns its file to remove.
pub fn delete_sound(conn: &Connection, id: SoundId) -> DbResult<Option<String>> {
    let file = get_sound(conn, id)?.map(|s| s.file);
    conn.execute("DELETE FROM sounds WHERE id = ?1", params![id])?;
    Ok(file)
}
