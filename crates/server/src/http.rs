//! File uploads and downloads over plain HTTP, next to the WebSocket.
//! Every request must carry `Authorization: Bearer <session token>`, and
//! files in direct messages can only be fetched by the two members.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
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

/// Only files up to this size get picture previews (bigger ones are just files).
const MAX_PREVIEW_SOURCE: u64 = 64 * 1024 * 1024;

fn new_id() -> String {
    rand::rng().random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect()
}

fn part_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.part"))
}

pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> HttpResult<Json<Attachment>> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let me = authenticate(&state, &headers).await?;
    let channel = q.channel_id;
    let kind = state
        .db
        .call(move |c| db::accessible_kind(c, me, channel))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?;
    if !matches!(kind, Some(ChannelKind::Text | ChannelKind::Direct)) {
        return Err(bad(StatusCode::FORBIDDEN, "you can't post files there"));
    }

    // Stream straight to disk, so a 1 GB upload doesn't need 1 GB of memory.
    let limit = state.config.max_upload_mb.saturating_mul(1024 * 1024);
    let dir = state.config.uploads.clone();
    let id = new_id();
    let part = part_path(&dir, &id);
    let save_error = |e: std::io::Error| {
        warn!("could not save upload to {}: {e}", dir.display());
        bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the file")
    };
    tokio::fs::create_dir_all(&dir).await.map_err(save_error)?;
    let mut file = tokio::fs::File::create(&part).await.map_err(save_error)?;
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    let outcome: HttpResult<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| bad(StatusCode::BAD_REQUEST, "the upload was interrupted"))?;
            size += chunk.len() as u64;
            if size > limit {
                return Err(bad(StatusCode::PAYLOAD_TOO_LARGE, "that file is bigger than this server allows"));
            }
            file.write_all(&chunk).await.map_err(save_error)?;
        }
        file.flush().await.map_err(save_error)?;
        if size == 0 {
            return Err(bad(StatusCode::BAD_REQUEST, "empty file"));
        }
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = outcome {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    finish_upload(&state, me, channel, &q.filename, &id, size).await.map(Json)
}

/// Store bytes fetched by the server itself (a GIF from GIF search) as an upload.
pub async fn store_bytes(
    state: &AppState,
    me: UserId,
    channel: ChannelId,
    filename: &str,
    bytes: &[u8],
) -> HttpResult<Attachment> {
    let id = new_id();
    let dir = &state.config.uploads;
    let write = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(part_path(dir, &id), bytes).await
    };
    write.await.map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the file"))?;
    finish_upload(state, me, channel, filename, &id, bytes.len() as u64).await
}

/// Make previews, move the finished file into place, and record it.
async fn finish_upload(
    state: &AppState,
    me: UserId,
    channel: ChannelId,
    filename: &str,
    id: &str,
    size: u64,
) -> HttpResult<Attachment> {
    let dir = &state.config.uploads;
    let part = part_path(dir, id);
    let filename = clean_filename(filename);
    let failed = |msg: &'static str| bad(StatusCode::INTERNAL_SERVER_ERROR, msg);

    let processed = if size <= MAX_PREVIEW_SOURCE {
        let (path, name) = (part.clone(), filename.clone());
        tokio::task::spawn_blocking(move || match std::fs::read(&path) {
            Ok(data) => media::process(&data, &name),
            Err(_) => media::Processed { mime: media::mime_from_name(&name).into(), ..Default::default() },
        })
        .await
        .map_err(|_| failed("couldn't read the file"))?
    } else {
        media::Processed { mime: media::mime_from_name(&filename).into(), ..Default::default() }
    };

    let write = async {
        if let Some(thumb) = &processed.thumb {
            tokio::fs::write(file_path(dir, id, true), thumb).await?;
        }
        tokio::fs::rename(&part, file_path(dir, id, false)).await
    };
    if let Err(e) = write.await {
        warn!("could not save upload to {}: {e}", dir.display());
        let _ = tokio::fs::remove_file(&part).await;
        remove_files(dir, &[id.to_string()]).await;
        return Err(failed("the server couldn't save the file"));
    }

    let attachment = Attachment {
        id: id.to_string(),
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
        remove_files(dir, &[id.to_string()]).await;
        return Err(failed("database error"));
    }
    Ok(attachment)
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
    let path = file_path(&state.config.uploads, &id, want_thumb);
    let missing = || bad(StatusCode::NOT_FOUND, "file is missing on the server");
    let (body, mime, len) = if want_thumb {
        // Previews are small; read them to work out their type.
        let bytes = tokio::fs::read(&path).await.map_err(|_| missing())?;
        let len = bytes.len() as u64;
        (Body::from(bytes.clone()), media::sniff_mime(&bytes).to_owned(), len)
    } else {
        // Originals can be huge: stream them from disk.
        let file = tokio::fs::File::open(&path).await.map_err(|_| missing())?;
        let len = file.metadata().await.map_err(|_| missing())?.len();
        (Body::from_stream(tokio_util::io::ReaderStream::new(file)), found.attachment.mime, len)
    };

    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    Ok(resp)
}
