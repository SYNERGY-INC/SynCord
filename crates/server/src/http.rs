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
use syncord_proto::{
    AVATAR_SIZE, Attachment, BANNER_SIZE, ChannelId, ChannelKind, ProfileImage, ServerMsg, UserId, clean_filename,
};
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
        video: None,
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

// ---- Profile pictures and banners

/// Largest profile picture or banner upload (animated GIFs can be big).
pub const MAX_PROFILE_IMAGE: u64 = 12 * 1024 * 1024;

#[derive(Deserialize)]
pub struct ProfileImageQuery {
    kind: String,
}

/// The app crops and resizes pictures before sending; check it did, so nobody can
/// make everyone download a huge file.
fn check_profile_image(bytes: &[u8], banner: bool) -> Result<(String, bool), &'static str> {
    let format = image::guess_format(bytes).map_err(|_| "that isn't a picture")?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err("use a PNG, JPEG, GIF or WebP picture"),
    };
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let (w, h) = reader.into_dimensions().map_err(|_| "that picture is damaged")?;
    let (max_w, max_h) =
        if banner { (BANNER_SIZE.0 * 2, BANNER_SIZE.1 * 2) } else { (AVATAR_SIZE * 2, AVATAR_SIZE * 2) };
    if w == 0 || h == 0 || w > max_w || h > max_h {
        return Err("that picture is the wrong size");
    }
    let animated = format == image::ImageFormat::Gif && media::is_animated_gif(bytes);
    Ok((mime.to_string(), animated))
}

pub async fn upload_profile_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ProfileImageQuery>,
    body: axum::body::Bytes,
) -> Result<Json<ProfileImage>, (StatusCode, &'static str)> {
    let me = authenticate(&state, &headers).await?;
    let banner = match q.kind.as_str() {
        "avatar" => false,
        "banner" => true,
        _ => return Err(bad(StatusCode::BAD_REQUEST, "kind must be avatar or banner")),
    };
    let (mime, animated) = check_profile_image(&body, banner).map_err(|m| bad(StatusCode::BAD_REQUEST, m))?;
    let id = new_id();
    let dir = &state.config.uploads;
    let save = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(file_path(dir, &id, false), &body).await
    };
    save.await.map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the picture"))?;

    let image = ProfileImage { id: id.clone(), animated };
    let stored = image.clone();
    let result = state
        .db
        .call(move |c| {
            db::insert_profile_image(c, &stored.id, me, &mime, stored.animated)?;
            let Some(mut user) = db::get_user(c, me)? else { return Ok(None) };
            let old = if banner { user.profile.banner.replace(stored) } else { user.profile.avatar.replace(stored) };
            db::update_profile(c, me, &user.profile)?;
            if let Some(old) = &old {
                db::delete_profile_image(c, &old.id)?;
            }
            Ok(Some((user, old)))
        })
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?;
    let Some((user, old)) = result else { return Err(bad(StatusCode::UNAUTHORIZED, "not logged in")) };
    if let Some(old) = old {
        remove_files(dir, &[old.id]).await;
    }
    state.broadcast(&ServerMsg::UserUpdated(user), None);
    Ok(Json(image))
}

pub async fn download_profile_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> HttpResult<Response> {
    authenticate(&state, &headers).await?;
    if !valid_id(&id) {
        return Err(bad(StatusCode::NOT_FOUND, "no such picture"));
    }
    let lookup = id.clone();
    let mime = state
        .db
        .call(move |c| db::profile_image_mime(c, &lookup))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?
        .ok_or(bad(StatusCode::NOT_FOUND, "no such picture"))?;
    let bytes = tokio::fs::read(file_path(&state.config.uploads, &id, false))
        .await
        .map_err(|_| bad(StatusCode::NOT_FOUND, "picture is missing on the server"))?;
    let mut resp = Body::from(bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/png")));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    Ok(resp)
}

/// Save a link preview's picture for a message (see [`crate::previews`]).
pub async fn store_embed_image(
    state: &AppState,
    author: UserId,
    channel: ChannelId,
    message: syncord_proto::MessageId,
    image: &crate::embeds::PreviewImage,
) -> Option<Attachment> {
    let id = new_id();
    let dir = &state.config.uploads;
    let write = async {
        tokio::fs::create_dir_all(dir).await?;
        if let Some(thumb) = &image.thumb {
            tokio::fs::write(file_path(dir, &id, true), thumb).await?;
        }
        tokio::fs::write(file_path(dir, &id, false), &image.bytes).await
    };
    if let Err(e) = write.await {
        warn!("could not save a link preview picture: {e}");
        return None;
    }
    let a = Attachment {
        id: id.clone(),
        filename: "preview".into(),
        size: image.bytes.len() as u64,
        mime: image.mime.clone(),
        width: Some(image.width),
        height: Some(image.height),
        animated: image.animated,
        video: None,
    };
    let (stored, has_thumb) = (a.clone(), image.thumb.is_some());
    match state.db.call(move |c| db::insert_embed_image(c, &stored, author, channel, message, has_thumb)).await {
        Ok(()) => Some(a),
        Err(e) => {
            warn!("could not record a link preview picture: {e}");
            remove_files(dir, &[id]).await;
            None
        }
    }
}
