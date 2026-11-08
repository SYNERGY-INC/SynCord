//! File uploads and downloads over plain HTTP, next to the WebSocket.
//! Every request must carry `Authorization: Bearer <session token>`, and
//! files in direct messages can only be fetched by the two members.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rand::Rng;
use serde::Deserialize;
use syncord_proto::{Attachment, ChannelId, ChannelKind, UserId, clean_filename};
use tracing::warn;

use crate::{AppState, db, media};

type HttpResult<T> = Result<T, (StatusCode, &'static str)>;

fn bad(code: StatusCode, msg: &'static str) -> (StatusCode, &'static str) {
    (code, msg)
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> HttpResult<UserId> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(bad(StatusCode::UNAUTHORIZED, "missing login token"))?
        .to_owned();
    state
        .db
        .call(move |c| db::session_user(c, &token))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?
        .ok_or(bad(StatusCode::UNAUTHORIZED, "not logged in"))
}

/// Attachment ids are 32 lowercase hex characters; anything else could escape the folder.
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn file_path(dir: &Path, id: &str, thumb: bool) -> PathBuf {
    dir.join(if thumb { format!("{id}.thumb") } else { id.to_owned() })
}

/// Delete stored files (originals and previews).
pub async fn remove_files(dir: &Path, ids: &[String]) {
    for id in ids.iter().filter(|id| valid_id(id)) {
        for thumb in [false, true] {
            let _ = tokio::fs::remove_file(file_path(dir, id, thumb)).await;
        }
    }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    channel_id: ChannelId,
    filename: String,
}

pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> HttpResult<Json<Attachment>> {
    let me = authenticate(&state, &headers).await?;
    if body.is_empty() {
        return Err(bad(StatusCode::BAD_REQUEST, "empty file"));
    }
    let channel = q.channel_id;
    let kind = state
        .db
        .call(move |c| db::accessible_kind(c, me, channel))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?;
    if !matches!(kind, Some(ChannelKind::Text | ChannelKind::Direct)) {
        return Err(bad(StatusCode::FORBIDDEN, "you can't post files there"));
    }

    let filename = clean_filename(&q.filename);
    let size = body.len() as u64;
    let name = filename.clone();
    let data = body.clone();
    let processed = tokio::task::spawn_blocking(move || media::process(&data, &name))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "couldn't read the file"))?;

    let id: String = rand::rng().random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect();
    let dir = &state.config.uploads;
    let write = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(file_path(dir, &id, false), &body).await?;
        if let Some(thumb) = &processed.thumb {
            tokio::fs::write(file_path(dir, &id, true), thumb).await?;
        }
        std::io::Result::Ok(())
    };
    if let Err(e) = write.await {
        warn!("could not save upload to {}: {e}", dir.display());
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the file"));
    }

    let attachment = Attachment {
        id: id.clone(),
        filename,
        size,
        mime: processed.mime,
        width: processed.width,
        height: processed.height,
        animated: processed.animated,
    };
    let has_thumb = processed.thumb.is_some();
    let a = attachment.clone();
    if state.db.call(move |c| db::insert_attachment(c, &a, me, channel, has_thumb)).await.is_err() {
        remove_files(dir, &[id]).await;
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"));
    }
    Ok(Json(attachment))
}

#[derive(Deserialize)]
pub struct DownloadQuery {
    #[serde(default)]
    thumb: Option<u8>,
}

pub async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<DownloadQuery>,
) -> HttpResult<Response> {
    let me = authenticate(&state, &headers).await?;
    if !valid_id(&id) {
        return Err(bad(StatusCode::NOT_FOUND, "no such file"));
    }
    let lookup = id.clone();
    let found = state
        .db
        .call(move |c| {
            let Some(a) = db::get_attachment(c, &lookup)? else { return Ok(None) };
            // Not sent yet: only the uploader may see it. Otherwise: whoever can see the channel.
            let allowed = match a.message_id {
                None => a.uploader == me,
                Some(_) => db::accessible_kind(c, me, a.channel_id)?.is_some(),
            };
            Ok(allowed.then_some(a))
        })
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?
        .ok_or(bad(StatusCode::NOT_FOUND, "no such file"))?;

    let want_thumb = q.thumb == Some(1) && found.has_thumb;
    let bytes = tokio::fs::read(file_path(&state.config.uploads, &id, want_thumb))
        .await
        .map_err(|_| bad(StatusCode::NOT_FOUND, "file is missing on the server"))?;
    let mime = if want_thumb { media::sniff_mime(&bytes).to_owned() } else { found.attachment.mime };

    let mut resp = Body::from(bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    Ok(resp)
}
