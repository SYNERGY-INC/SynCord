//! Uploading and downloading custom emoji, stickers and soundboard sounds over HTTP
//! (see [`celerity_proto::expressions`]). Renaming and deleting go over the WebSocket.

use std::io::Cursor;
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use celerity_proto::expressions::{
    EMOJI_SIZE, MAX_EMOJI, MAX_EMOJI_BYTES, MAX_SOUND_BYTES, MAX_SOUND_MS, MAX_SOUNDS, MAX_STICKER_BYTES, MAX_STICKERS,
    STICKER_SIZE, valid_expression_name,
};
use celerity_proto::{CustomEmoji, ServerMsg, Sound, Sticker, perms};
use image::{ImageFormat, ImageReader};
use serde::Deserialize;
use tracing::warn;

use crate::db::{self, PictureKind};
use crate::http::{HttpResult, authenticate, bad, file_path, new_id, remove_files};
use crate::{AppState, audio, media};

/// Largest picture accepted for shrinking down to an emoji or sticker.
pub const MAX_SOURCE_PICTURE: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
pub struct NameQuery {
    name: String,
    #[serde(default)]
    emoji: Option<String>,
}

/// Limits for one kind of picture.
struct PictureRules {
    kind: PictureKind,
    max_bytes: usize,
    max_side: u32,
    max_count: usize,
    what: &'static str,
}

const EMOJI_RULES: PictureRules = PictureRules {
    kind: PictureKind::Emoji,
    max_bytes: MAX_EMOJI_BYTES,
    max_side: EMOJI_SIZE,
    max_count: MAX_EMOJI,
    what: "emoji",
};

const STICKER_RULES: PictureRules = PictureRules {
    kind: PictureKind::Sticker,
    max_bytes: MAX_STICKER_BYTES,
    max_side: STICKER_SIZE,
    max_count: MAX_STICKERS,
    what: "sticker",
};

/// Check a picture, shrinking still pictures that are too big. Returns the bytes to keep,
/// their type and whether they move.
pub fn prepare_picture(
    bytes: &[u8],
    max_bytes: usize,
    max_side: u32,
) -> Result<(Vec<u8>, &'static str, bool), &'static str> {
    let format = image::guess_format(bytes).map_err(|_| "that isn't a picture")?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return Err("use a PNG, GIF or WebP picture"),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let (w, h) = reader.into_dimensions().map_err(|_| "that picture is damaged or too big")?;
    if w == 0 || h == 0 {
        return Err("that picture is damaged");
    }
    let animated = format == ImageFormat::Gif && media::is_animated_gif(bytes);
    if w <= max_side && h <= max_side && bytes.len() <= max_bytes {
        return Ok((bytes.to_vec(), mime, animated));
    }
    if animated {
        return Err("animated pictures must already be small enough: shrink it and try again");
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let img = reader.decode().map_err(|_| "that picture is damaged")?;
    let small = if w > max_side || h > max_side { img.thumbnail(max_side, max_side) } else { img };
    let mut out = Cursor::new(Vec::new());
    small.to_rgba8().write_to(&mut out, ImageFormat::Png).map_err(|_| "couldn't shrink that picture")?;
    let out = out.into_inner();
    if out.len() > max_bytes {
        return Err("that picture is too big, even shrunk down");
    }
    Ok((out, "image/png", false))
}

async fn upload_picture(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    name: String,
    body: Bytes,
    rules: PictureRules,
) -> HttpResult<db::StoredPicture> {
    let me = authenticate(state, headers).await?;
    let name = name.trim().to_string();
    if !valid_expression_name(&name) {
        return Err(bad(StatusCode::BAD_REQUEST, "names are 2-32 letters, numbers and underscores"));
    }
    if body.len() > MAX_SOURCE_PICTURE {
        return Err(bad(StatusCode::PAYLOAD_TOO_LARGE, "that picture is too big"));
    }
    let n = name.clone();
    let refusal = state
        .db
        .call(move |c| {
            Ok(if !db::access(c, me)?.can(perms::MANAGE_EMOJI) {
                Some((StatusCode::FORBIDDEN, "you need the Manage emoji permission"))
            } else if db::picture_count(c, rules.kind)? >= rules.max_count {
                Some((StatusCode::CONFLICT, "this server has as many of those as it can have"))
            } else if db::picture_name_taken(c, rules.kind, &n, 0)? {
                Some((StatusCode::CONFLICT, "that name is already taken"))
            } else {
                None
            })
        })
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?;
    if let Some(why) = refusal {
        return Err(why);
    }
    let (max_bytes, max_side) = (rules.max_bytes, rules.max_side);
    let (bytes, mime, animated) = tokio::task::spawn_blocking(move || prepare_picture(&body, max_bytes, max_side))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "couldn't read the picture"))?
        .map_err(|m| bad(StatusCode::BAD_REQUEST, m))?;
    let file = new_id();
    let dir = &state.config.uploads;
    let save = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(file_path(dir, &file, false), &bytes).await
    };
    if let Err(e) = save.await {
        warn!("could not save {}: {e}", rules.what);
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the picture"));
    }
    let f = file.clone();
    match state.db.call(move |c| db::insert_picture(c, rules.kind, &name, &f, mime, animated, me)).await {
        Ok(p) => Ok(p),
        Err(_) => {
            remove_files(dir, &[file]).await;
            Err(bad(StatusCode::CONFLICT, "that name is already taken"))
        }
    }
}

pub async fn upload_emoji(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<NameQuery>,
    body: Bytes,
) -> HttpResult<Json<CustomEmoji>> {
    let stored = upload_picture(&state, &headers, q.name, body, EMOJI_RULES).await?;
    let emoji = stored.emoji();
    state.broadcast(&ServerMsg::EmojiUpdated(emoji.clone()), None);
    Ok(Json(emoji))
}

pub async fn upload_sticker(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<NameQuery>,
    body: Bytes,
) -> HttpResult<Json<Sticker>> {
    let stored = upload_picture(&state, &headers, q.name, body, STICKER_RULES).await?;
    let sticker = stored.sticker();
    state.broadcast(&ServerMsg::StickerUpdated(sticker.clone()), None);
    Ok(Json(sticker))
}

/// A sound's name: 1–32 characters, no control characters (spaces are fine).
pub fn clean_sound_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(32).collect()
}

/// A sound's button emoji, if it's a sensible one.
pub fn clean_sound_emoji(emoji: Option<String>) -> Option<String> {
    emoji.map(|e| e.trim().to_string()).filter(|e| celerity_proto::valid_reaction(e))
}

pub async fn upload_sound(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<NameQuery>,
    body: Bytes,
) -> HttpResult<Json<Sound>> {
    let me = authenticate(&state, &headers).await?;
    let name = clean_sound_name(&q.name);
    if name.is_empty() {
        return Err(bad(StatusCode::BAD_REQUEST, "give the sound a name"));
    }
    if body.len() > MAX_SOUND_BYTES {
        return Err(bad(StatusCode::PAYLOAD_TOO_LARGE, "sounds can be at most 1 MB"));
    }
    let info = audio::inspect(&body).map_err(|m| bad(StatusCode::BAD_REQUEST, m))?;
    // A little slack for encoders that pad the end.
    if info.duration_ms > MAX_SOUND_MS + 200 {
        return Err(bad(StatusCode::BAD_REQUEST, "sounds can be at most 5 seconds long"));
    }
    let refusal = state
        .db
        .call(move |c| {
            Ok(if !db::access(c, me)?.can(perms::MANAGE_SOUNDBOARD) {
                Some((StatusCode::FORBIDDEN, "you need the Manage soundboard permission"))
            } else if db::sound_count(c)? >= MAX_SOUNDS {
                Some((StatusCode::CONFLICT, "this server has as many sounds as it can have"))
            } else {
                None
            })
        })
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?;
    if let Some(why) = refusal {
        return Err(why);
    }
    let file = new_id();
    let dir = &state.config.uploads;
    let save = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(file_path(dir, &file, false), &body).await
    };
    if save.await.is_err() {
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "the server couldn't save the sound"));
    }
    let sound = Sound {
        id: 0,
        name,
        emoji: clean_sound_emoji(q.emoji),
        mime: info.mime.into(),
        duration_ms: info.duration_ms,
        size: body.len() as u64,
        created_by: me,
    };
    let f = file.clone();
    let sound = match state.db.call(move |c| db::insert_sound(c, &sound, &f)).await {
        Ok(s) => s,
        Err(_) => {
            remove_files(dir, &[file]).await;
            return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"));
        }
    };
    state.broadcast(&ServerMsg::SoundUpdated(sound.clone()), None);
    Ok(Json(sound))
}

async fn serve(state: &AppState, file: &str, mime: &str) -> HttpResult<Response> {
    let bytes = tokio::fs::read(file_path(&state.config.uploads, file, false))
        .await
        .map_err(|_| bad(StatusCode::NOT_FOUND, "the file is missing on the server"))?;
    let mut resp = Body::from(bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // Ids can come back after a deletion, so don't cache forever.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    Ok(resp)
}

async fn download_picture(state: &AppState, headers: &HeaderMap, id: i64, kind: PictureKind) -> HttpResult<Response> {
    authenticate(state, headers).await?;
    let found = state
        .db
        .call(move |c| db::get_picture(c, kind, id))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?
        .ok_or(bad(StatusCode::NOT_FOUND, "no such picture"))?;
    serve(state, &found.file, &found.mime).await
}

pub async fn download_emoji(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> HttpResult<Response> {
    download_picture(&state, &headers, id, PictureKind::Emoji).await
}

pub async fn download_sticker(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> HttpResult<Response> {
    download_picture(&state, &headers, id, PictureKind::Sticker).await
}

pub async fn download_sound(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<i64>,
) -> HttpResult<Response> {
    authenticate(&state, &headers).await?;
    let found = state
        .db
        .call(move |c| db::get_sound(c, id))
        .await
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "database error"))?
        .ok_or(bad(StatusCode::NOT_FOUND, "no such sound"))?;
    serve(&state, &found.file, &found.sound.mime).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn pictures_are_checked_and_shrunk() {
        let (bytes, mime, animated) = prepare_picture(&png(64, 64), MAX_EMOJI_BYTES, EMOJI_SIZE).unwrap();
        assert_eq!((mime, animated), ("image/png", false));
        assert_eq!(bytes, png(64, 64), "small enough: kept as it is");
        let (bytes, _, _) = prepare_picture(&png(512, 256), MAX_EMOJI_BYTES, EMOJI_SIZE).unwrap();
        let shrunk = image::load_from_memory(&bytes).unwrap();
        assert_eq!((shrunk.width(), shrunk.height()), (128, 64));
        assert!(prepare_picture(b"GIF89a nonsense", MAX_EMOJI_BYTES, EMOJI_SIZE).is_err());
        assert!(prepare_picture(b"plain text", MAX_EMOJI_BYTES, EMOJI_SIZE).is_err());
        assert_eq!(clean_sound_name("  air   horn\n"), "air horn");
        assert_eq!(clean_sound_emoji(Some("a b".into())), None);
    }
}
