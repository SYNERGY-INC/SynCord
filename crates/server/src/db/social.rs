//! 0.7 storage: private channels and categories, threads, group direct messages and polls.

use std::collections::{HashMap, HashSet};

use celerity_proto::{
    Category, CategoryId, CategoryPosition, Channel, ChannelId, ChannelKind, ChannelPosition, EVERYONE_ROLE,
    EncryptedPayload, GroupChannel, MessageId, Poll, RoleId, Thread, ThreadSummary, UserId, perms,
};
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    DbResult, access, all_channels, channel_kind, create_channel, direct_members, kind_to_i64, now_ms, string_column,
    user_roles,
};

// ---- Private channels

fn role_list(conn: &Connection, sql: &str, id: i64) -> DbResult<Vec<RoleId>> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.query_map(params![id], |r| r.get(0))?.collect()
}

/// The roles a channel itself is limited to (empty: none of its own).
pub fn channel_roles(conn: &Connection, channel: ChannelId) -> DbResult<Vec<RoleId>> {
    role_list(conn, "SELECT role_id FROM channel_roles WHERE channel_id = ?1 ORDER BY role_id", channel)
}

pub fn category_roles(conn: &Connection, category: CategoryId) -> DbResult<Vec<RoleId>> {
    role_list(conn, "SELECT role_id FROM category_roles WHERE category_id = ?1 ORDER BY role_id", category)
}

/// Every channel's own role list, for channels that have one.
pub fn channel_role_lists(conn: &Connection) -> DbResult<HashMap<ChannelId, Vec<RoleId>>> {
    let mut out: HashMap<ChannelId, Vec<RoleId>> = HashMap::new();
    let mut stmt = conn.prepare_cached("SELECT channel_id, role_id FROM channel_roles ORDER BY role_id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, ChannelId>(0)?, r.get::<_, RoleId>(1)?)))? {
        let (c, r) = row?;
        out.entry(c).or_default().push(r);
    }
    Ok(out)
}

fn set_roles(conn: &Connection, table: &str, column: &str, id: i64, roles: &[RoleId]) -> DbResult<()> {
    conn.execute(&format!("DELETE FROM {table} WHERE {column} = ?1"), params![id])?;
    for r in roles {
        // Unknown roles are skipped (the foreign key would refuse them).
        conn.execute(
            &format!("INSERT OR IGNORE INTO {table} ({column}, role_id) SELECT ?1, id FROM roles WHERE id = ?2"),
            params![id, r],
        )?;
    }
    Ok(())
}

pub fn set_channel_roles(conn: &Connection, channel: ChannelId, roles: &[RoleId]) -> DbResult<()> {
    set_roles(conn, "channel_roles", "channel_id", channel, roles)
}

pub fn set_category_roles(conn: &Connection, category: CategoryId, roles: &[RoleId]) -> DbResult<()> {
    set_roles(conn, "category_roles", "category_id", category, roles)
}

/// The thread's channel, if `id` is a thread.
pub fn thread_parent(conn: &Connection, id: ChannelId) -> DbResult<Option<ChannelId>> {
    conn.query_row("SELECT parent_id FROM channels WHERE id = ?1 AND kind = 4", params![id], |r| r.get(0))
        .optional()
        .map(Option::flatten)
}

/// Who a server channel is limited to: its own roles, or else its category's. Empty: everyone.
pub fn effective_roles(conn: &Connection, channel: ChannelId) -> DbResult<Vec<RoleId>> {
    let own = channel_roles(conn, channel)?;
    if !own.is_empty() {
        return Ok(own);
    }
    let category: Option<CategoryId> = conn
        .query_row("SELECT category_id FROM channels WHERE id = ?1", params![channel], |r| r.get(0))
        .optional()?
        .flatten();
    match category {
        Some(c) => category_roles(conn, c),
        None => Ok(Vec::new()),
    }
}

/// Whether someone with these roles may see something limited to `allowed`.
fn roles_allow(allowed: &[RoleId], theirs: &[RoleId], admin: bool) -> bool {
    allowed.is_empty() || admin || allowed.contains(&EVERYONE_ROLE) || theirs.iter().any(|r| allowed.contains(r))
}

fn is_admin(conn: &Connection, user: UserId) -> DbResult<bool> {
    Ok(access(conn, user)?.can(perms::ADMINISTRATOR))
}

/// Whether `user` may see a server channel (or, for a thread, its channel).
pub fn can_see_channel(conn: &Connection, user: UserId, channel: ChannelId) -> DbResult<bool> {
    let channel = thread_parent(conn, channel)?.unwrap_or(channel);
    let allowed = effective_roles(conn, channel)?;
    if allowed.is_empty() {
        return Ok(true);
    }
    Ok(roles_allow(&allowed, &user_roles(conn, user)?, is_admin(conn, user)?))
}

/// Members who aren't kicked or banned.
fn member_ids(conn: &Connection) -> DbResult<Vec<UserId>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM users WHERE removed = 0 ORDER BY id")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// Who gets a channel's events: `None` for everyone (public server channels), otherwise
/// the list (private channels and their threads, direct messages and groups).
pub fn channel_audience(conn: &Connection, channel: ChannelId) -> DbResult<Option<Vec<UserId>>> {
    match channel_kind(conn, channel)? {
        Some(ChannelKind::Direct | ChannelKind::Group) => Ok(Some(direct_members(conn, channel)?)),
        Some(ChannelKind::Thread) => match thread_parent(conn, channel)? {
            Some(parent) => roles_audience(conn, &effective_roles(conn, parent)?),
            None => Ok(Some(Vec::new())),
        },
        Some(_) => roles_audience(conn, &effective_roles(conn, channel)?),
        None => Ok(None),
    }
}

fn roles_audience(conn: &Connection, allowed: &[RoleId]) -> DbResult<Option<Vec<UserId>>> {
    if allowed.is_empty() || allowed.contains(&EVERYONE_ROLE) {
        return Ok(None);
    }
    let mut out = Vec::new();
    for u in member_ids(conn)? {
        if roles_allow(allowed, &user_roles(conn, u)?, is_admin(conn, u)?) {
            out.push(u);
        }
    }
    Ok(Some(out))
}

/// The text and voice channels `user` can see.
pub fn channels_for(conn: &Connection, user: UserId) -> DbResult<Vec<Channel>> {
    let theirs = user_roles(conn, user)?;
    let admin = is_admin(conn, user)?;
    let categories: HashMap<CategoryId, Vec<RoleId>> =
        all_categories(conn)?.into_iter().map(|c| (c.id, c.allowed_roles)).collect();
    Ok(all_channels(conn)?
        .into_iter()
        .filter(|c| {
            let allowed = if c.allowed_roles.is_empty() {
                c.category_id.and_then(|id| categories.get(&id)).map(Vec::as_slice).unwrap_or(&[])
            } else {
                &c.allowed_roles
            };
            roles_allow(allowed, &theirs, admin)
        })
        .collect())
}

/// Which text and voice channels each member can see, to tell people about channels
/// they gained or lost sight of after a change.
pub fn visibility_map(conn: &Connection) -> DbResult<Visibility> {
    let mut out = Visibility::default();
    for u in member_ids(conn)? {
        out.channels.insert(u, channels_for(conn, u)?.into_iter().map(|c| c.id).collect());
        out.categories.insert(u, categories_for(conn, u)?.into_iter().map(|c| c.id).collect());
    }
    Ok(out)
}

/// What each member can see (see [`visibility_map`]).
#[derive(Debug, Clone, Default)]
pub struct Visibility {
    pub channels: HashMap<UserId, HashSet<ChannelId>>,
    pub categories: HashMap<UserId, HashSet<CategoryId>>,
}

/// Channels whose messages `user` may search: visible text channels and their threads,
/// and their direct messages and groups.
pub fn searchable_channels(conn: &Connection, user: UserId) -> DbResult<Vec<ChannelId>> {
    let text: HashSet<ChannelId> =
        channels_for(conn, user)?.into_iter().filter(|c| c.kind == ChannelKind::Text).map(|c| c.id).collect();
    let mut out: Vec<ChannelId> = text.iter().copied().collect();
    let mut stmt = conn.prepare_cached("SELECT id, parent_id FROM channels WHERE kind = 4")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, ChannelId>(0)?, r.get::<_, Option<ChannelId>>(1)?)))? {
        let (id, parent) = row?;
        if parent.is_some_and(|p| text.contains(&p)) {
            out.push(id);
        }
    }
    out.extend(string_ids(conn, "SELECT channel_id FROM direct_members WHERE user_id = ?1", user)?);
    out.sort_unstable();
    Ok(out)
}

fn string_ids(conn: &Connection, sql: &str, arg: i64) -> DbResult<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.query_map(params![arg], |r| r.get(0))?.collect()
}

// ---- Categories

/// All categories, in order.
pub fn all_categories(conn: &Connection) -> DbResult<Vec<Category>> {
    let mut stmt = conn.prepare_cached("SELECT id, name, position FROM categories ORDER BY position, id")?;
    let mut cats: Vec<Category> = stmt
        .query_map([], |r| Ok(Category { id: r.get(0)?, name: r.get(1)?, position: r.get(2)?, allowed_roles: vec![] }))?
        .collect::<DbResult<_>>()?;
    for c in &mut cats {
        c.allowed_roles = category_roles(conn, c.id)?;
    }
    Ok(cats)
}

pub fn get_category(conn: &Connection, id: CategoryId) -> DbResult<Option<Category>> {
    Ok(all_categories(conn)?.into_iter().find(|c| c.id == id))
}

/// The categories `user` can see.
pub fn categories_for(conn: &Connection, user: UserId) -> DbResult<Vec<Category>> {
    let theirs = user_roles(conn, user)?;
    let admin = is_admin(conn, user)?;
    Ok(all_categories(conn)?.into_iter().filter(|c| roles_allow(&c.allowed_roles, &theirs, admin)).collect())
}

/// Who sees a category (`None`: everyone).
pub fn category_audience(conn: &Connection, id: CategoryId) -> DbResult<Option<Vec<UserId>>> {
    roles_audience(conn, &category_roles(conn, id)?)
}

pub fn create_category(conn: &Connection, name: &str) -> DbResult<Category> {
    let position: i64 = conn.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM categories", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO categories (name, position, created_at) VALUES (?1, ?2, ?3)",
        params![name, position, now_ms()],
    )?;
    Ok(Category { id: conn.last_insert_rowid(), name: name.into(), position, allowed_roles: vec![] })
}

pub fn rename_category(conn: &Connection, id: CategoryId, name: &str) -> DbResult<()> {
    conn.execute("UPDATE categories SET name = ?1 WHERE id = ?2", params![name, id])?;
    Ok(())
}

/// Deletes a category; its channels move to the top level. Returns them.
pub fn delete_category(conn: &Connection, id: CategoryId) -> DbResult<Vec<ChannelId>> {
    let tx = conn.unchecked_transaction()?;
    let moved = string_ids(&tx, "SELECT id FROM channels WHERE category_id = ?1", id)?;
    tx.execute("UPDATE channels SET category_id = NULL WHERE category_id = ?1", params![id])?;
    tx.execute("DELETE FROM categories WHERE id = ?1", params![id])?;
    tx.commit()?;
    Ok(moved)
}

pub fn set_channel_category(conn: &Connection, channel: ChannelId, category: Option<CategoryId>) -> DbResult<()> {
    conn.execute("UPDATE channels SET category_id = ?1 WHERE id = ?2 AND kind IN (0, 1)", params![category, channel])?;
    Ok(())
}

pub fn update_channel(conn: &Connection, id: ChannelId, name: &str, topic: &str) -> DbResult<()> {
    conn.execute("UPDATE channels SET name = ?1, topic = ?2 WHERE id = ?3", params![name, topic, id])?;
    Ok(())
}

/// Apply a drag and drop: positions and categories, all or nothing. Unknown channels and
/// categories are refused (`Ok(false)`), so a stale list can't half-apply.
pub fn reorder(conn: &Connection, categories: &[CategoryPosition], channels: &[ChannelPosition]) -> DbResult<bool> {
    let tx = conn.unchecked_transaction()?;
    for c in categories {
        if tx.execute("UPDATE categories SET position = ?1 WHERE id = ?2", params![c.position, c.id])? != 1 {
            return Ok(false);
        }
    }
    for c in channels {
        if let Some(cat) = c.category_id {
            let exists =
                tx.query_row("SELECT 1 FROM categories WHERE id = ?1", params![cat], |_| Ok(())).optional()?.is_some();
            if !exists {
                return Ok(false);
            }
        }
        let changed = tx.execute(
            "UPDATE channels SET position = ?1, category_id = ?2 WHERE id = ?3 AND kind IN (0, 1)",
            params![c.position, c.category_id, c.id],
        )?;
        if changed != 1 {
            return Ok(false);
        }
    }
    tx.commit()?;
    Ok(true)
}

// ---- Threads

const THREAD_COLUMNS: &str = "c.id, c.parent_id, c.parent_message_id, c.name, c.owner_id, c.archived, c.created_at,
    (SELECT COUNT(*) FROM messages m WHERE m.channel_id = c.id),
    (SELECT MAX(m.id) FROM messages m WHERE m.channel_id = c.id),
    (SELECT MAX(m.created_at) FROM messages m WHERE m.channel_id = c.id)";

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let created_at: i64 = r.get(6)?;
    Ok(Thread {
        id: r.get(0)?,
        parent_id: r.get::<_, Option<ChannelId>>(1)?.unwrap_or_default(),
        parent_message_id: r.get(2)?,
        name: r.get(3)?,
        owner_id: r.get::<_, Option<UserId>>(4)?.unwrap_or_default(),
        archived: r.get(5)?,
        created_at,
        message_count: r.get::<_, i64>(7)? as u32,
        last_message_id: r.get(8)?,
        last_activity: r.get::<_, Option<i64>>(9)?.unwrap_or(created_at),
    })
}

pub fn create_thread(
    conn: &Connection,
    parent: ChannelId,
    parent_message: Option<MessageId>,
    name: &str,
    owner: UserId,
) -> DbResult<Thread> {
    let tx = conn.unchecked_transaction()?;
    let channel = create_channel(&tx, name, "", ChannelKind::Thread)?;
    tx.execute(
        "UPDATE channels SET parent_id = ?1, parent_message_id = ?2, owner_id = ?3, position = 0 WHERE id = ?4",
        params![parent, parent_message, owner, channel.id],
    )?;
    tx.commit()?;
    Ok(get_thread(conn, channel.id)?.expect("just made"))
}

pub fn get_thread(conn: &Connection, id: ChannelId) -> DbResult<Option<Thread>> {
    conn.query_row(
        &format!("SELECT {THREAD_COLUMNS} FROM channels c WHERE c.id = ?1 AND c.kind = 4"),
        params![id],
        row_to_thread,
    )
    .optional()
}

/// A channel's threads, most recently active first.
pub fn threads_in(conn: &Connection, parent: ChannelId) -> DbResult<Vec<Thread>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {THREAD_COLUMNS} FROM channels c WHERE c.kind = 4 AND c.parent_id = ?1"))?;
    let mut threads: Vec<Thread> = stmt.query_map(params![parent], row_to_thread)?.collect::<DbResult<_>>()?;
    threads.sort_by_key(|t| std::cmp::Reverse(t.last_activity));
    Ok(threads)
}

/// Threads that aren't archived, in every channel.
pub fn active_threads(conn: &Connection) -> DbResult<Vec<Thread>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {THREAD_COLUMNS} FROM channels c WHERE c.kind = 4 AND c.archived = 0"))?;
    stmt.query_map([], row_to_thread)?.collect()
}

/// The thread started from a message, if there is one.
pub fn thread_for_message(conn: &Connection, message: MessageId) -> DbResult<Option<ChannelId>> {
    conn.query_row("SELECT id FROM channels WHERE kind = 4 AND parent_message_id = ?1", params![message], |r| r.get(0))
        .optional()
}

pub fn set_thread(conn: &Connection, id: ChannelId, name: &str, archived: bool) -> DbResult<()> {
    conn.execute(
        "UPDATE channels SET name = ?1, archived = ?2 WHERE id = ?3 AND kind = 4",
        params![name, archived, id],
    )?;
    Ok(())
}

/// Summaries of the threads started from messages `lo..=hi` of a channel.
pub(super) fn thread_summaries(
    conn: &Connection,
    channel: ChannelId,
    lo: MessageId,
    hi: MessageId,
) -> DbResult<HashMap<MessageId, ThreadSummary>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREAD_COLUMNS} FROM channels c
         WHERE c.kind = 4 AND c.parent_id = ?1 AND c.parent_message_id BETWEEN ?2 AND ?3"
    ))?;
    let mut out = HashMap::new();
    for t in stmt.query_map(params![channel, lo, hi], row_to_thread)? {
        let t = t?;
        if let Some(m) = t.parent_message_id {
            out.insert(m, t.summary());
        }
    }
    Ok(out)
}

// ---- Group direct messages

/// Make a group owned by `owner` with `others` in it.
pub fn create_group(conn: &Connection, owner: UserId, others: &[UserId], name: &str) -> DbResult<ChannelId> {
    let tx = conn.unchecked_transaction()?;
    let channel = create_channel(&tx, name, "", ChannelKind::Group)?;
    tx.execute("UPDATE channels SET owner_id = ?1 WHERE id = ?2", params![owner, channel.id])?;
    tx.execute("INSERT INTO direct_members (channel_id, user_id) VALUES (?1, ?2)", params![channel.id, owner])?;
    for u in others {
        tx.execute(
            "INSERT OR IGNORE INTO direct_members (channel_id, user_id) VALUES (?1, ?2)",
            params![channel.id, u],
        )?;
    }
    tx.commit()?;
    Ok(channel.id)
}

pub fn get_group(conn: &Connection, id: ChannelId) -> DbResult<Option<GroupChannel>> {
    let row = conn
        .query_row(
            "SELECT name, owner_id, (SELECT MAX(m.id) FROM messages m WHERE m.channel_id = channels.id)
             FROM channels WHERE id = ?1 AND kind = ?2",
            params![id, kind_to_i64(ChannelKind::Group)],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<UserId>>(1)?, r.get::<_, Option<MessageId>>(2)?)),
        )
        .optional()?;
    let Some((name, owner, last)) = row else { return Ok(None) };
    Ok(Some(GroupChannel {
        id,
        name,
        owner_id: owner.unwrap_or_default(),
        members: direct_members(conn, id)?,
        last_message_id: last,
    }))
}

/// Someone's groups, newest activity first.
pub fn groups_for(conn: &Connection, user: UserId) -> DbResult<Vec<GroupChannel>> {
    let ids = string_ids(
        conn,
        "SELECT d.channel_id FROM direct_members d JOIN channels c ON c.id = d.channel_id AND c.kind = 3
         WHERE d.user_id = ?1",
        user,
    )?;
    let mut out = Vec::new();
    for id in ids {
        out.extend(get_group(conn, id)?);
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.last_message_id.unwrap_or(0)));
    Ok(out)
}

pub fn add_group_member(conn: &Connection, group: ChannelId, user: UserId) -> DbResult<()> {
    conn.execute("INSERT OR IGNORE INTO direct_members (channel_id, user_id) VALUES (?1, ?2)", params![group, user])?;
    Ok(())
}

/// Take someone out of a group. If the owner goes, whoever has been in it longest takes
/// over; if nobody is left, the group is deleted (returns its files to remove).
pub fn remove_group_member(conn: &Connection, group: ChannelId, user: UserId) -> DbResult<Vec<String>> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM direct_members WHERE channel_id = ?1 AND user_id = ?2", params![group, user])?;
    // Their read marker goes too, so rejoining later starts fresh.
    tx.execute("DELETE FROM read_states WHERE channel_id = ?1 AND user_id = ?2", params![group, user])?;
    let left = direct_members(&tx, group)?;
    let mut files = Vec::new();
    if left.is_empty() {
        files = string_column(&tx, "SELECT id FROM attachments WHERE channel_id = ?1", group)?;
        tx.execute("DELETE FROM channels WHERE id = ?1", params![group])?;
    } else {
        tx.execute("UPDATE channels SET owner_id = ?1 WHERE id = ?2 AND owner_id = ?3", params![left[0], group, user])?;
    }
    tx.commit()?;
    Ok(files)
}

pub fn rename_group(conn: &Connection, group: ChannelId, name: &str) -> DbResult<()> {
    conn.execute("UPDATE channels SET name = ?1 WHERE id = ?2 AND kind = 3", params![name, group])?;
    Ok(())
}

// ---- Polls

/// Fill in a poll's votes, and close it if its time is up.
pub fn fill_poll(conn: &Connection, message: MessageId, poll: &mut Poll) -> DbResult<()> {
    let mut stmt =
        conn.prepare_cached("SELECT option, user_id FROM poll_votes WHERE message_id = ?1 ORDER BY rowid")?;
    for o in &mut poll.options {
        o.votes.clear();
    }
    for row in stmt.query_map(params![message], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, UserId>(1)?)))? {
        let (option, user) = row?;
        if let Some(o) = poll.options.iter_mut().find(|o| o.id == option) {
            o.votes.push(user);
        }
    }
    if poll.ends_at.is_some_and(|t| t <= now_ms()) {
        poll.closed = true;
    }
    Ok(())
}

/// The poll of a message, with its votes.
pub fn get_poll(conn: &Connection, message: MessageId) -> DbResult<Option<Poll>> {
    let json: Option<String> =
        conn.query_row("SELECT poll FROM messages WHERE id = ?1", params![message], |r| r.get(0)).optional()?.flatten();
    let Some(mut poll) = json.and_then(|j| serde_json::from_str::<Poll>(&j).ok()) else { return Ok(None) };
    fill_poll(conn, message, &mut poll)?;
    Ok(Some(poll))
}

/// Add or take back a vote. Without `multiple`, a vote replaces the person's other votes.
/// Returns whether anything changed.
pub fn vote(
    conn: &Connection,
    message: MessageId,
    option: u32,
    user: UserId,
    on: bool,
    multiple: bool,
) -> DbResult<bool> {
    let tx = conn.unchecked_transaction()?;
    let mut changed = 0;
    if on {
        if !multiple {
            changed += tx.execute(
                "DELETE FROM poll_votes WHERE message_id = ?1 AND user_id = ?2 AND option != ?3",
                params![message, user, option],
            )?;
        }
        changed += tx.execute(
            "INSERT OR IGNORE INTO poll_votes (message_id, option, user_id) VALUES (?1, ?2, ?3)",
            params![message, option, user],
        )?;
    } else {
        changed += tx.execute(
            "DELETE FROM poll_votes WHERE message_id = ?1 AND option = ?2 AND user_id = ?3",
            params![message, option, user],
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

/// Mark a poll as ended (its results are final).
pub fn close_poll(conn: &Connection, message: MessageId) -> DbResult<()> {
    conn.execute(
        "UPDATE messages SET poll = json_set(poll, '$.closed', json('true')) WHERE id = ?1 AND poll IS NOT NULL",
        params![message],
    )?;
    Ok(())
}

/// Polls whose time ran out but that haven't been marked as ended yet.
pub fn due_polls(conn: &Connection, now: i64) -> DbResult<Vec<MessageId>> {
    string_ids(
        conn,
        "SELECT id FROM messages WHERE poll IS NOT NULL
         AND json_extract(poll, '$.closed') = 0 AND json_extract(poll, '$.ends_at') <= ?1",
        now,
    )
}

// ---- Encrypted messages

pub fn edit_encrypted(conn: &Connection, id: MessageId, payload: &EncryptedPayload) -> DbResult<()> {
    conn.execute(
        "UPDATE messages SET encrypted = ?1, edited_at = ?2 WHERE id = ?3 AND encrypted IS NOT NULL",
        params![serde_json::to_string(payload).unwrap(), now_ms(), id],
    )?;
    Ok(())
}
