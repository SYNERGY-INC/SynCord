//! Link previews for messages: after a message with links is sent (or edited),
//! fetch the pages in the background, store their pictures, and update the
//! message for everyone who can see it.

use std::sync::Arc;

use celerity_proto::{Embed, Message, ServerMsg};
use tracing::debug;

use crate::{AppState, db, embeds, http};

/// Make previews for a new message, or redo them after an edit changed its links.
/// `previous` is the text before an edit. Encrypted messages never get previews: the
/// server can't (and shouldn't) read them.
pub fn update(state: Arc<AppState>, message: Message, previous: Option<&str>) {
    if message.encrypted.is_some() {
        return;
    }
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
            let rich = p.rich.clone().unwrap_or_default();
            embeds.push(Embed {
                url: p.url.clone(),
                site_name: p.site_name.clone(),
                title: p.title.clone(),
                description: p.description.clone(),
                color: p.color,
                image,
                media: p.is_media,
                provider: p.rich.as_ref().map(|r| r.provider.to_string()),
                author: rich.author,
                author_url: rich.author_url,
                duration_secs: rich.duration_secs,
                embed_id: rich.embed_id,
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
        if let Some(m) = updated
            && let Ok(to) = state.channel_target(m.channel_id).await
        {
            state.broadcast_target(&ServerMsg::MessageUpdated(m), to);
        }
    });
}
