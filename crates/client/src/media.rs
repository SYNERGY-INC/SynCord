//! Pictures and files: HTTP uploads/downloads, decoding (including animated
//! GIFs), and a cache that keeps memory use within a budget.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

use iced::widget::image::Handle;
use image::{AnimationDecoder, ImageFormat, codecs::gif::GifDecoder, imageops::FilterType};
use syncord_proto::{Attachment, ChannelId, FILES_PATH, UPLOAD_PATH};

/// Longest side for pictures in the chat (server previews are at most 480 px).
pub const THUMB_SIDE: u32 = 480;
/// Longest side for the full-screen viewer.
pub const VIEWER_SIDE: u32 = 2048;
/// Most animation frames kept in memory for one GIF.
const MAX_FRAMES: usize = 150;
/// Decoded pictures kept in memory, in bytes, before old ones are dropped.
const CACHE_BUDGET: usize = 96 * 1024 * 1024;

/// `ws://host:port/ws` -> `http://host:port` (and `wss` -> `https`).
pub fn http_base(ws_url: &str) -> String {
    let url = ws_url.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    match url.find("://").and_then(|i| url[i + 3..].find('/').map(|j| i + 3 + j)) {
        Some(path_start) => url[..path_start].to_string(),
        None => url,
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10)).build().unwrap_or_default()
}

fn describe(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        401 => "You're not logged in.".into(),
        403 => "You can't post files here.".into(),
        404 => "That file isn't available any more.".into(),
        413 => "That file is too big for this server.".into(),
        code => format!("The server answered with error {code}."),
    }
}

pub async fn upload(
    http: reqwest::Client,
    base: String,
    token: String,
    channel: ChannelId,
    filename: String,
    bytes: Vec<u8>,
) -> Result<Attachment, String> {
    let url = reqwest::Url::parse_with_params(
        &format!("{base}{UPLOAD_PATH}"),
        &[("channel_id", channel.to_string()), ("filename", filename)],
    )
    .map_err(|e| format!("Bad server address: {e}"))?;
    let resp = http.post(url).bearer_auth(token).body(bytes).send().await.map_err(|e| format!("Upload failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(describe(resp.status()));
    }
    let body = resp.bytes().await.map_err(|e| format!("Upload failed: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("Unexpected answer from the server: {e}"))
}

pub async fn download(
    http: reqwest::Client,
    base: String,
    token: String,
    id: String,
    thumb: bool,
) -> Result<Vec<u8>, String> {
    let url = format!("{base}{FILES_PATH}/{id}{}", if thumb { "?thumb=1" } else { "" });
    let resp = http.get(url).bearer_auth(token).send().await.map_err(|e| format!("Download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(describe(resp.status()));
    }
    Ok(resp.bytes().await.map_err(|e| format!("Download failed: {e}"))?.to_vec())
}

/// A decoded picture, ready to draw.
#[derive(Debug, Clone)]
pub enum Picture {
    Still { handle: Handle, width: u32, height: u32 },
    Animated { frames: Vec<(Handle, u32)>, width: u32, height: u32, total_ms: u64 },
}

impl Picture {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Picture::Still { width, height, .. } | Picture::Animated { width, height, .. } => (*width, *height),
        }
    }

    fn memory(&self) -> usize {
        let (w, h) = self.size();
        let frames = match self {
            Picture::Still { .. } => 1,
            Picture::Animated { frames, .. } => frames.len(),
        };
        w as usize * h as usize * 4 * frames
    }

    /// The frame to show `elapsed_ms` into the animation (or the still picture).
    pub fn frame(&self, elapsed_ms: u64) -> &Handle {
        match self {
            Picture::Still { handle, .. } => handle,
            Picture::Animated { frames, total_ms, .. } => {
                let mut t = elapsed_ms % (*total_ms).max(1);
                for (handle, delay) in frames {
                    if t < *delay as u64 {
                        return handle;
                    }
                    t -= *delay as u64;
                }
                &frames[0].0
            }
        }
    }
}

fn shrink(img: image::RgbaImage, max_side: u32) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= max_side {
        return img;
    }
    let scale = max_side as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    image::imageops::resize(&img, nw, nh, FilterType::Triangle)
}

fn still(img: image::RgbaImage) -> Picture {
    let (width, height) = img.dimensions();
    Picture::Still { handle: Handle::from_rgba(width, height, img.into_raw()), width, height }
}

/// Decode an image (animated GIFs keep their frames), shrinking it to `max_side`.
pub fn decode(bytes: &[u8], max_side: u32) -> Result<Picture, String> {
    if matches!(image::guess_format(bytes), Ok(ImageFormat::Gif)) {
        let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let mut frames = Vec::new();
        let mut total_ms = 0u64;
        for frame in decoder.into_frames().take(MAX_FRAMES) {
            let Ok(frame) = frame else { break };
            let (n, d) = frame.delay().numer_denom_ms();
            // Browsers treat very short delays as 100 ms; do the same so GIFs play at the usual speed.
            let delay = match n / d.max(1) {
                0..=10 => 100,
                ms => ms,
            };
            let img = shrink(frame.into_buffer(), max_side);
            let (w, h) = img.dimensions();
            total_ms += delay as u64;
            frames.push((Handle::from_rgba(w, h, img.into_raw()), delay, w, h));
        }
        return match frames.len() {
            0 => Err("empty GIF".into()),
            1 => {
                let (handle, _, width, height) = frames.pop().unwrap();
                Ok(Picture::Still { handle, width, height })
            }
            _ => {
                let (width, height) = (frames[0].2, frames[0].3);
                let frames = frames.into_iter().map(|(h, d, _, _)| (h, d)).collect();
                Ok(Picture::Animated { frames, width, height, total_ms })
            }
        };
    }
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let img = if img.width().max(img.height()) > max_side { img.thumbnail(max_side, max_side) } else { img };
    Ok(still(img.to_rgba8()))
}

/// Download and decode a picture without blocking the UI.
pub async fn load(
    http: reqwest::Client,
    base: String,
    token: String,
    id: String,
    thumb: bool,
) -> Result<Arc<Picture>, String> {
    let bytes = download(http, base, token, id, thumb).await?;
    let max_side = if thumb { THUMB_SIDE } else { VIEWER_SIDE };
    tokio::task::spawn_blocking(move || decode(&bytes, max_side)).await.map_err(|e| e.to_string())?.map(Arc::new)
}

/// Cache key for an attachment's preview or full picture.
pub fn key(id: &str, full: bool) -> String {
    if full { format!("{id}:full") } else { id.to_string() }
}

pub enum Entry {
    Loading,
    Ready(Arc<Picture>),
    Failed,
}

/// Decoded pictures, dropping the least recently used ones beyond a memory budget.
#[derive(Default)]
pub struct ImageCache {
    entries: HashMap<String, (Entry, Instant)>,
}

impl ImageCache {
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key).map(|(e, _)| e)
    }

    pub fn picture(&self, key: &str) -> Option<&Arc<Picture>> {
        match self.get(key) {
            Some(Entry::Ready(p)) => Some(p),
            _ => None,
        }
    }

    /// Mark as loading; returns false if it's already loaded or on its way.
    pub fn start(&mut self, key: &str) -> bool {
        match self.entries.get_mut(key) {
            Some((Entry::Ready(_) | Entry::Loading, used)) => {
                *used = Instant::now();
                false
            }
            _ => {
                self.entries.insert(key.to_string(), (Entry::Loading, Instant::now()));
                true
            }
        }
    }

    pub fn finish(&mut self, key: String, result: Result<Arc<Picture>, String>) {
        let entry = match result {
            Ok(p) => Entry::Ready(p),
            Err(_) => Entry::Failed,
        };
        self.entries.insert(key, (entry, Instant::now()));
        self.evict();
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }

    pub fn memory(&self) -> usize {
        self.entries
            .values()
            .map(|(e, _)| match e {
                Entry::Ready(p) => p.memory(),
                _ => 0,
            })
            .sum()
    }

    fn evict(&mut self) {
        while self.memory() > CACHE_BUDGET {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, (e, _))| matches!(e, Entry::Ready(_)))
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    pub fn has_animation(&self, keys: &[String]) -> bool {
        keys.iter().any(|k| matches!(self.picture(k).map(|p| &**p), Some(Picture::Animated { .. })))
    }
}

/// Human-friendly file size.
pub fn format_size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.0} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Size to draw a picture in the chat, keeping its shape within a box.
pub fn fit(width: u32, height: u32, max_w: f32, max_h: f32) -> (f32, f32) {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (max_w / w).min(max_h / h).min(1.0);
    ((w * scale).max(16.0), (h * scale).max(16.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};

    #[test]
    fn urls() {
        assert_eq!(http_base("ws://100.1.2.3:7878/ws"), "http://100.1.2.3:7878");
        assert_eq!(http_base("wss://chat.example.com/ws"), "https://chat.example.com");
    }

    #[test]
    fn decodes_and_shrinks() {
        let img = RgbaImage::from_pixel(1000, 500, Rgba([1, 2, 3, 255]));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, ImageFormat::Png).unwrap();
        let p = decode(&png.into_inner(), 480).unwrap();
        assert_eq!(p.size(), (480, 240));
        assert!(matches!(p, Picture::Still { .. }));
    }

    #[test]
    fn animated_gif_frames_and_timing() {
        let frames: Vec<Frame> = (0..3)
            .map(|i| {
                let img = RgbaImage::from_pixel(10, 10, Rgba([i * 80, 0, 0, 255]));
                Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(200, 1))
            })
            .collect();
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.encode_frames(frames).unwrap();
        }
        let p = decode(&out, 480).unwrap();
        let Picture::Animated { frames, total_ms, .. } = &p else { panic!("expected animation") };
        assert_eq!(frames.len(), 3);
        assert_eq!(*total_ms, 600);
        assert_eq!(p.frame(0), &frames[0].0);
        assert_eq!(p.frame(250), &frames[1].0);
        assert_eq!(p.frame(650), &frames[0].0, "loops");
    }

    #[test]
    fn cache_respects_budget() {
        let mut cache = ImageCache::default();
        let big = || {
            let img = RgbaImage::new(2048, 2048); // 16 MB each
            Arc::new(still(img))
        };
        for i in 0..10 {
            let k = format!("k{i}");
            assert!(cache.start(&k));
            cache.finish(k, Ok(big()));
        }
        assert!(cache.memory() <= CACHE_BUDGET);
        assert!(cache.picture("k9").is_some(), "newest kept");
        assert!(cache.picture("k0").is_none(), "oldest dropped");
        assert!(!cache.start("k9"), "already loaded");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fit(1000, 500, 400.0, 300.0), (400.0, 200.0));
        assert_eq!(fit(100, 100, 400.0, 300.0), (100.0, 100.0));
    }
}
