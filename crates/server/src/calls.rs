//! Calls in direct messages leave a note in the conversation, like Discord:
//! "Sam started a call", later showing how long it lasted or that it was missed.

use std::sync::Arc;

use syncord_proto::{ChannelId, ChannelKind, MessageId, MessageKind, ServerMsg, UserId};
use tracing::warn;

use crate::{AppState, db};

/// Active call notes per direct-message channel.
#[derive(Default)]
pub struct Calls {
    active: std::sync::Mutex<std::collections::HashMap<ChannelId, MessageId>>,
}

async fn members_if_direct(state: &AppState, channel: ChannelId) -> Option<Vec<UserId>> {
    state
        .db
        .call(move |c| match db::channel_kind(c, channel)? {
            Some(ChannelKind::Direct) => Ok(Some(db::direct_members(c, channel)?)),
            _ => Ok(None),
        })
        .await
        .ok()
        .flatten()
}

/// Someone joined voice in `channel`: start a call note, or add them to the current one.
pub async fn joined(state: &Arc<AppState>, channel: ChannelId, user: UserId) {
    let Some(members) = members_if_direct(state, channel).await else { return };
    let to: Arc<[UserId]> = Arc::from(members.clone());
    let current = state.calls.active.lock().unwrap().get(&channel).copied();
    let result = match current {
        Some(id) => state
            .db
            .call(move |c| {
                let Some(m) = db::get_message(c, id)? else { return Ok(None) };
                if let MessageKind::Call { mut participants, ended_at } = m.kind {
                    if !participants.contains(&user) {
                        participants.push(user);
                        db::set_kind(c, id, &MessageKind::Call { participants, ended_at })?;
                    }
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
                let Ok(m) = db::insert_message(c, channel, user, &text, &[])? else { return Ok(None) };
                db::set_kind(c, m.id, &MessageKind::Call { participants: vec![user], ended_at: None })?;
                // The other person gets a ping for it.
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
            state.broadcast(&msg, Some(to));
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
    let Some(members) = members_if_direct(state, channel).await else { return };
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
        Ok(Some(m)) => state.broadcast(&ServerMsg::MessageUpdated(m), Some(Arc::from(members))),
        Ok(None) => {}
        Err(e) => warn!("couldn't record the end of a call: {e}"),
    }
}
