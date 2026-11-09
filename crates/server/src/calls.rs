//! Calls in direct messages and groups leave a note in the conversation, like Discord:
//! "Sam started a call", later showing how long it lasted or that it was missed. The note
//! pings everyone else in the conversation, which is what makes their apps ring; anyone in
//! it can join or leave while the call goes on.

use std::sync::Arc;

use celerity_proto::{ChannelId, ChannelKind, MessageId, MessageKind, ServerMsg, UserId};
use tracing::warn;

use crate::{AppState, Target, db};

/// Active call notes per direct-message channel.
#[derive(Default)]
pub struct Calls {
    active: std::sync::Mutex<std::collections::HashMap<ChannelId, MessageId>>,
}

/// The members of a direct message or group (and whether it's a group).
async fn members_if_direct(state: &AppState, channel: ChannelId) -> Option<(Vec<UserId>, bool)> {
    state
        .db
        .call(move |c| match db::channel_kind(c, channel)? {
            Some(ChannelKind::Direct) => Ok(Some((db::direct_members(c, channel)?, false))),
            Some(ChannelKind::Group) => Ok(Some((db::direct_members(c, channel)?, true))),
            _ => Ok(None),
        })
        .await
        .ok()
        .flatten()
}

/// Someone joined voice in `channel`: start a call note, or add them to the current one.
pub async fn joined(state: &Arc<AppState>, channel: ChannelId, user: UserId) {
    let Some((members, group)) = members_if_direct(state, channel).await else { return };
    let to = Target::only(members.clone()).modern(group);
    let current = state.calls.active.lock().unwrap().get(&channel).copied();
    let result = match current {
        Some(id) => state
            .db
            .call(move |c| {
                let Some(m) = db::get_message(c, id)? else { return Ok(None) };
                if let MessageKind::Call { mut participants, ended_at } = m.kind
                    && !participants.contains(&user)
                {
                    participants.push(user);
                    db::set_kind(c, id, &MessageKind::Call { participants, ended_at })?;
                }
                db::get_message(c, id)
            })
            .await
            .map(|m| m.map(ServerMsg::MessageUpdated)),
        None => state
            .db
            .call(move |c| {
                let name = db::get_user(c, user)?.map(|u| u.name().to_string()).unwrap_or_default();
                // What apps older than 0.6 show.
                let text = format!("📞 {name} started a call.");
                let Ok(m) = db::insert_message(c, channel, user, &text, &[], None)? else { return Ok(None) };
                db::set_kind(c, m.id, &MessageKind::Call { participants: vec![user], ended_at: None })?;
                // Everyone else gets a ping for it (their apps ring).
                let others: Vec<UserId> = members.iter().copied().filter(|u| *u != user).collect();
                db::set_mentions(c, m.id, channel, &others)?;
                db::mark_read(c, user, channel, m.id)?;
                db::get_message(c, m.id)
            })
            .await
            .map(|m| m.map(ServerMsg::MessageCreated)),
    };
    match result {
        Ok(Some(msg)) => {
            if let ServerMsg::MessageCreated(m) = &msg {
                state.calls.active.lock().unwrap().insert(channel, m.id);
            }
            state.broadcast_target(&msg, to);
        }
        Ok(None) => {}
        Err(e) => warn!("couldn't record a call: {e}"),
    }
}

/// Someone left voice in `channel`: when nobody's left, the call is over.
pub async fn left(state: &Arc<AppState>, channel: ChannelId) {
    if state.voice.states().iter().any(|v| v.channel_id == channel) {
        return;
    }
    let Some(id) = state.calls.active.lock().unwrap().remove(&channel) else { return };
    let Some((members, group)) = members_if_direct(state, channel).await else { return };
    let updated = state
        .db
        .call(move |c| {
            let Some(m) = db::get_message(c, id)? else { return Ok(None) };
            if let MessageKind::Call { participants, .. } = m.kind {
                db::set_kind(c, id, &MessageKind::Call { participants, ended_at: Some(db::now_ms()) })?;
            }
            db::get_message(c, id)
        })
        .await;
    match updated {
        Ok(Some(m)) => state.broadcast_target(&ServerMsg::MessageUpdated(m), Target::only(members).modern(group)),
        Ok(None) => {}
        Err(e) => warn!("couldn't record the end of a call: {e}"),
    }
}
