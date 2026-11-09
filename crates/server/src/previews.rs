//! Link previews for messages: after a message with links is sent (or edited),
//! fetch the pages in the background, store their pictures, and update the
//! message for everyone who can see it.

use std::sync::Arc;

use celerity_proto::{ChannelKind, Embed, Message, ServerMsg, UserId};
use tracing::debug;

use crate::{AppState, db, embeds, http};

/// Who sees a channel: its two members for a direct message, otherwise everyone.
pub async fn audience_of(state: &AppState, message: &Message) -> Option<Arc<[UserId]>> {
    let channel = message.channel_id;
    let members = state
        .db
        .call(move |c| match db::channel_kind(c, channel)? {
            Some(ChannelKind::Direct) => Ok(Some(db::direct_members(c, channel)?)),
            _ => Ok(None),
        })
        .await
        .ok()
        .flatten();
    members.map(Arc::from)
}

/// Make previews for a new message, or redo them after an edit changed its links.
/// `previous` is the text before an edit.
pub fn update(state: Arc<AppState>, message: Message, previous: Option<&str>) {
    let urls = embeds::find_urls(&message.content);
    if let Some(before) = previous
        && embeds::find_urls(before) == urls
    {
        return; // same links as before: keep the previews
    }
    if urls.is_empty() && message.embeds.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut previews = Vec::new();
        for url in &urls {
            match state.previews.fetch(url).await {
                Ok(Some(p)) => previews.push(p),
                Ok(None) => {}
                Err(e) => debug!("no preview for {url}: {e}"),
            }
        }
        let (id, channel, author) = (message.id, message.channel_id, message.author_id);
        // Pictures from earlier previews of this message are replaced.
        let old = state.db.call(move |c| db::take_embed_images(c, id)).await.unwrap_or_default();
        http::remove_files(&state.config.uploads, &old).await;

        let mut embeds = Vec::new();
        for p in previews {
            let image = match &p.image {
                Some(img) => http::store_embed_image(&state, author, channel, id, img).await,
                None => None,
            };
            if image.is_none() && p.title.is_none() && p.description.is_none() {
                continue;
            }
            embeds.push(Embed {
                url: p.url.clone(),
                site_name: p.site_name.clone(),
                title: p.title.clone(),
                description: p.description.clone(),
                color: p.color,
                image,
                media: p.is_media,
            });
        }
        let updated = state
            .db
            .call(move |c| {
                if db::get_message(c, id)?.is_none() {
                    return Ok(None); // deleted meanwhile
                }
                db::set_embeds(c, id, &embeds)?;
                db::get_message(c, id)
            })
            .await
            .ok()
            .flatten();
        if let Some(m) = updated {
            let to = audience_of(&state, &m).await;
            state.broadcast(&ServerMsg::MessageUpdated(m), to);
        }
    });
}
