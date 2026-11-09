//! Kicking and banning, shared by the app (WebSocket requests) and the admin web page.
//!
//! Accounts belong to the server, so a kicked person's account stays (their old messages
//! keep a name) but they're signed out and can't log in. Registering again with their
//! username, password and a valid invite brings them back. Banned people can't come back
//! until someone unbans them.

use std::sync::Arc;

use celerity_proto::{ServerMsg, UserId, perms};
use rusqlite::Connection;
use tracing::info;

use crate::{AppState, db};

/// Whether `actor` ranks strictly above `target`: the owner is above everyone, and
/// otherwise their highest role must be higher (nobody is above the owner).
pub fn strictly_outranks(conn: &Connection, actor: UserId, target: UserId) -> db::DbResult<bool> {
    if actor == target {
        return Ok(false);
    }
    let a = db::access(conn, actor)?;
    let t = db::access(conn, target)?;
    Ok(!t.is_owner && (a.is_owner || t.top < a.top))
}

/// Kick (`ban = false`) or ban someone on `actor`'s behalf. Errors are friendly sentences.
pub async fn remove(state: &Arc<AppState>, actor: UserId, target: UserId, ban: bool) -> Result<(), String> {
    let checked = state
        .db
        .call(move |c| {
            let needed = if ban { perms::BAN_MEMBERS } else { perms::KICK_MEMBERS };
            if !db::access(c, actor)?.can(needed) {
                return Ok(Err(if ban {
                    "You need the Ban members permission"
                } else {
                    "You need the Kick members permission"
                }));
            }
            let Some(user) = db::get_user(c, target)? else { return Ok(Err("That person doesn't exist")) };
            if target == actor {
                return Ok(Err("You can't remove yourself"));
            }
            if user.is_owner {
                return Ok(Err("Nobody can remove the server owner"));
            }
            if !strictly_outranks(c, actor, target)? {
                return Ok(Err("You can only remove people ranked below you"));
            }
            if user.banned || (user.removed && !ban) {
                return Ok(Ok(None)); // nothing to do
            }
            db::remove_member(c, target, ban)?;
            Ok(Ok(db::get_user(c, target)?))
        })
        .await
        .map_err(|e| {
            tracing::warn!("database error: {e}");
            "Server error, please try again".to_string()
        })?
        .map_err(str::to_string)?;
    let Some(user) = checked else { return Ok(()) };
    info!("user {actor} {} user {target}", if ban { "banned" } else { "kicked" });
    state.admin.forget_user(target);
    if let Some(channel) = state.voice.remove_user(target) {
        let to = state.channel_target(channel).await.unwrap_or_default();
        state.broadcast_target(&ServerMsg::VoiceStateUpdate { user_id: target, state: None }, to);
        crate::calls::left(state, channel).await;
    }
    state.broadcast_and_disconnect(&ServerMsg::MemberRemoved { user_id: target, banned: ban }, target);
    state.broadcast(&ServerMsg::UserUpdated(user), None);
    Ok(())
}

/// Lift a ban. They still need a new invite to come back, like someone who was kicked.
pub async fn unban(state: &Arc<AppState>, actor: UserId, target: UserId) -> Result<(), String> {
    let done = state
        .db
        .call(move |c| {
            if !db::access(c, actor)?.can(perms::BAN_MEMBERS) {
                return Ok(Err("You need the Ban members permission"));
            }
            if db::membership(c, target)? != Some(db::BANNED) {
                return Ok(Err("That person isn't banned"));
            }
            db::set_membership(c, target, db::KICKED)?;
            Ok(Ok(db::get_user(c, target)?))
        })
        .await
        .map_err(|_| "Server error, please try again".to_string())?
        .map_err(str::to_string)?;
    info!("user {actor} unbanned user {target}");
    if let Some(user) = done {
        state.broadcast(&ServerMsg::UserUpdated(user), None);
    }
    Ok(())
}
