//! 0.7 storage: invite links, kicks and bans, and published identity keys.

use celerity_proto::{IdentityKeys, Invite, UserId};
use rusqlite::{Connection, OptionalExtension, params};

use super::{BANNED, DbResult, KICKED, MEMBER, now_ms};

// ---- Invite links

fn row_to_invite(r: &rusqlite::Row<'_>) -> rusqlite::Result<Invite> {
    Ok(Invite {
        token: r.get(0)?,
        created_by: r.get(1)?,
        created_at: r.get(2)?,
        expires_at: r.get(3)?,
        max_uses: r.get(4)?,
        uses: r.get(5)?,
    })
}

const INVITE_COLUMNS: &str = "token, created_by, created_at, expires_at, max_uses, uses";

pub fn create_invite(
    conn: &Connection,
    token: &str,
    by: UserId,
    expires_at: Option<i64>,
    max_uses: Option<u32>,
) -> DbResult<Invite> {
    let created_at = now_ms();
    conn.execute(
        "INSERT INTO invites (token, created_by, created_at, expires_at, max_uses) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![token, by, created_at, expires_at, max_uses],
    )?;
    Ok(Invite { token: token.into(), created_by: by, created_at, expires_at, max_uses, uses: 0 })
}

pub fn get_invite(conn: &Connection, token: &str) -> DbResult<Option<Invite>> {
    conn.query_row(&format!("SELECT {INVITE_COLUMNS} FROM invites WHERE token = ?1"), params![token], row_to_invite)
        .optional()
}

/// Invites that still work, newest first. Used-up and expired ones are tidied away.
pub fn usable_invites(conn: &Connection) -> DbResult<Vec<Invite>> {
    let now = now_ms();
    conn.execute(
        "DELETE FROM invites WHERE (expires_at IS NOT NULL AND expires_at <= ?1)
         OR (max_uses IS NOT NULL AND uses >= max_uses)",
        params![now],
    )?;
    let mut stmt = conn.prepare_cached(&format!("SELECT {INVITE_COLUMNS} FROM invites ORDER BY created_at DESC"))?;
    stmt.query_map([], row_to_invite)?.collect()
}

pub fn revoke_invite(conn: &Connection, token: &str) -> DbResult<bool> {
    Ok(conn.execute("DELETE FROM invites WHERE token = ?1", params![token])? > 0)
}

/// Count one use of an invite, if it still works. Returns whether it did.
pub fn use_invite(conn: &Connection, token: &str) -> DbResult<bool> {
    let used = conn.execute(
        "UPDATE invites SET uses = uses + 1 WHERE token = ?1
         AND (expires_at IS NULL OR expires_at > ?2) AND (max_uses IS NULL OR uses < max_uses)",
        params![token, now_ms()],
    )?;
    Ok(used == 1)
}

/// Give back a use (the account couldn't be made after all).
pub fn unuse_invite(conn: &Connection, token: &str) -> DbResult<()> {
    conn.execute("UPDATE invites SET uses = MAX(uses - 1, 0) WHERE token = ?1", params![token])?;
    Ok(())
}

// ---- Kicks and bans

/// Kick (`ban = false`) or ban someone: they lose their roles and are signed out everywhere.
pub fn remove_member(conn: &Connection, user: UserId, ban: bool) -> DbResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE users SET removed = ?1 WHERE id = ?2", params![if ban { BANNED } else { KICKED }, user])?;
    tx.execute("DELETE FROM user_roles WHERE user_id = ?1", params![user])?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?1", params![user])?;
    tx.commit()
}

/// Let someone back in: a kicked person who registered again, or an unban (they then
/// count as kicked, so they still need an invite to come back).
pub fn set_membership(conn: &Connection, user: UserId, state: i64) -> DbResult<()> {
    debug_assert!([MEMBER, KICKED, BANNED].contains(&state));
    conn.execute("UPDATE users SET removed = ?1 WHERE id = ?2", params![state, user])?;
    Ok(())
}

pub fn membership(conn: &Connection, user: UserId) -> DbResult<Option<i64>> {
    conn.query_row("SELECT removed FROM users WHERE id = ?1", params![user], |r| r.get(0)).optional()
}

pub fn banned_users(conn: &Connection) -> DbResult<Vec<UserId>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM users WHERE removed = ?1 ORDER BY id")?;
    stmt.query_map(params![BANNED], |r| r.get(0))?.collect()
}

// ---- Identity keys

fn row_to_keys(r: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityKeys> {
    Ok(IdentityKeys {
        user_id: r.get(0)?,
        agreement_key: r.get(1)?,
        signing_key: r.get(2)?,
        signature: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

pub fn set_identity_keys(conn: &Connection, keys: &IdentityKeys) -> DbResult<()> {
    conn.execute(
        "INSERT INTO identity_keys (user_id, agreement_key, signing_key, signature, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(user_id) DO UPDATE SET agreement_key = excluded.agreement_key,
             signing_key = excluded.signing_key, signature = excluded.signature, updated_at = excluded.updated_at",
        params![keys.user_id, keys.agreement_key, keys.signing_key, keys.signature, keys.updated_at],
    )?;
    Ok(())
}

pub fn all_identity_keys(conn: &Connection) -> DbResult<Vec<IdentityKeys>> {
    let mut stmt = conn.prepare_cached(
        "SELECT user_id, agreement_key, signing_key, signature, updated_at FROM identity_keys ORDER BY user_id",
    )?;
    stmt.query_map([], row_to_keys)?.collect()
}

pub fn identity_keys_of(conn: &Connection, users: &[UserId]) -> DbResult<Vec<IdentityKeys>> {
    Ok(all_identity_keys(conn)?.into_iter().filter(|k| users.contains(&k.user_id)).collect())
}
