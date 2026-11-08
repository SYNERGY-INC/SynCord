//! Works out what an uploaded file is and makes a small preview for images,
//! so apps only ever download (and keep in memory) small pictures.

use std::io::Cursor;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{AnimationDecoder, DynamicImage, Frame, ImageFormat, ImageReader, Limits, imageops::FilterType};

/// Largest preview for still images.
const THUMB_W: u32 = 480;
const THUMB_H: u32 = 360;
/// Largest preview for animated GIFs (smaller: every frame is kept in memory by the app).
const GIF_THUMB: u32 = 320;
/// Most frames kept in a GIF preview; longer GIFs keep every n-th frame.
const MAX_GIF_FRAMES: usize = 120;
/// Refuse to decode images bigger than this many pixels (per frame).
const MAX_PIXELS: u64 = 50_000_000;
/// Stop decoding a GIF after this many pixels in total across frames.
const MAX_GIF_PIXELS: u64 = 400_000_000;
/// Small images are shown as-is instead of making a separate preview.
const SMALL_FILE: usize = 256 * 1024;

#[derive(Debug, Default)]
pub struct Processed {
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub animated: bool,
    /// Preview bytes (JPEG, PNG or GIF), if one was made.
    pub thumb: Option<Vec<u8>>,
}

/// Look at an upload and make a preview if it's a picture.
/// Anything that isn't a readable image is treated as a plain file.
pub fn process(bytes: &[u8], filename: &str) -> Processed {
    let format = image::guess_format(bytes).ok();
    let result = match format {
        Some(ImageFormat::Gif) => process_gif(bytes),
        Some(f @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Bmp)) => {
            process_still(bytes, f)
        }
        _ => None,
    };
    result.unwrap_or_else(|| Processed { mime: mime_from_name(filename).into(), ..Default::default() })
}

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(20_000);
    l.max_image_height = Some(20_000);
    l.max_alloc = Some(512 * 1024 * 1024);
    l
}

fn process_still(bytes: &[u8], format: ImageFormat) -> Option<Processed> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits());
    let (w, h) = reader.into_dimensions().ok()?;
    if w as u64 * h as u64 > MAX_PIXELS {
        return None;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits());
    let img = reader.decode().ok()?;

    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => "image/bmp",
    };
    let small = w <= THUMB_W && h <= THUMB_H && bytes.len() <= SMALL_FILE && format != ImageFormat::Bmp;
    let thumb = if small { None } else { encode_still_thumb(&img) };
    Some(Processed { mime: mime.into(), width: Some(w), height: Some(h), animated: false, thumb })
}

fn encode_still_thumb(img: &DynamicImage) -> Option<Vec<u8>> {
    let thumb =
        if img.width() > THUMB_W || img.height() > THUMB_H { img.thumbnail(THUMB_W, THUMB_H) } else { img.clone() };
    let mut out = Cursor::new(Vec::new());
    // Keep PNG only if something is actually see-through; JPEG is far smaller otherwise.
    let transparent = thumb.color().has_alpha() && thumb.to_rgba8().pixels().any(|p| p[3] < 255);
    if transparent {
        thumb.to_rgba8().write_to(&mut out, ImageFormat::Png).ok()?;
    } else {
        let rgb = DynamicImage::ImageRgb8(thumb.to_rgb8());
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
        enc.encode_image(&rgb).ok()?;
    }
    Some(out.into_inner())
}

fn process_gif(bytes: &[u8]) -> Option<Processed> {
    let decoder = GifDecoder::new(Cursor::new(bytes)).ok()?;
    let mut frames: Vec<Frame> = Vec::new();
    let mut pixels = 0u64;
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        let (w, h) = frame.buffer().dimensions();
        if w as u64 * h as u64 > MAX_PIXELS {
            return None;
        }
        pixels += w as u64 * h as u64;
        frames.push(frame);
        if pixels > MAX_GIF_PIXELS || frames.len() >= MAX_GIF_FRAMES * 4 {
            break;
        }
    }
    let first = frames.first()?;
    let (w, h) = first.buffer().dimensions();
    let animated = frames.len() > 1;

    let thumb = if !animated {
        encode_still_thumb(&DynamicImage::ImageRgba8(first.buffer().clone()))
    } else if w <= GIF_THUMB && h <= GIF_THUMB && frames.len() <= MAX_GIF_FRAMES {
        None // already small enough to use as its own preview
    } else {
        encode_gif_thumb(frames)
    };
    Some(Processed { mime: "image/gif".into(), width: Some(w), height: Some(h), animated, thumb })
}

/// Shrink an animation and drop frames so the preview stays small.
fn encode_gif_thumb(frames: Vec<Frame>) -> Option<Vec<u8>> {
    let step = frames.len().div_ceil(MAX_GIF_FRAMES).max(1);
    let (w, h) = frames[0].buffer().dimensions();
    let scale = (GIF_THUMB as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));

    let mut kept = Vec::new();
    for chunk in frames.chunks(step) {
        // Keep the first frame of each group, lasting as long as the whole group.
        let delay_ms: u32 = chunk
            .iter()
            .map(|f| {
                let (n, d) = f.delay().numer_denom_ms();
                n / d.max(1)
            })
            .sum();
        let buf = chunk[0].buffer();
        let buf = if scale < 1.0 { image::imageops::resize(buf, tw, th, FilterType::Triangle) } else { buf.clone() };
        kept.push(Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(delay_ms.max(20), 1)));
    }

    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 20);
        enc.set_repeat(Repeat::Infinite).ok()?;
        enc.encode_frames(kept).ok()?;
    }
    Some(out)
}

/// Content type for a file that isn't an image, from its extension.
pub fn mime_from_name(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "txt" | "log" | "md" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Content type of a stored preview, from its first bytes.
pub fn sniff_mime(bytes: &[u8]) -> &'static str {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => "image/png",
        Ok(ImageFormat::Jpeg) => "image/jpeg",
        Ok(ImageFormat::Gif) => "image/gif",
        Ok(ImageFormat::WebP) => "image/webp",
        Ok(ImageFormat::Bmp) => "image/bmp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn gif(w: u32, h: u32, n: usize) -> Vec<u8> {
        let frames: Vec<Frame> = (0..n)
            .map(|i| {
                let img = RgbaImage::from_pixel(w, h, Rgba([(i * 20 % 256) as u8, 0, 0, 255]));
                Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(50, 1))
            })
            .collect();
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new_with_speed(&mut out, 30);
            enc.set_repeat(Repeat::Infinite).unwrap();
            enc.encode_frames(frames).unwrap();
        }
        out
    }

    #[test]
    fn big_image_gets_a_jpeg_preview() {
        let p = process(&png(2000, 1000), "photo.png");
        assert_eq!(p.mime, "image/png");
        assert_eq!((p.width, p.height), (Some(2000), Some(1000)));
        let thumb = p.thumb.expect("preview");
        let t = image::load_from_memory(&thumb).unwrap();
        assert!(t.width() <= THUMB_W && t.height() <= THUMB_H);
        assert_eq!(t.width(), 480);
    }

    #[test]
    fn transparent_image_keeps_png_preview() {
        let img = RgbaImage::from_fn(1000, 1000, |x, _| Rgba([0, 0, 0, if x < 500 { 0 } else { 255 }]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        let p = process(&out.into_inner(), "logo.png");
        assert_eq!(sniff_mime(&p.thumb.unwrap()), "image/png");
    }

    #[test]
    fn small_image_is_its_own_preview() {
        let p = process(&png(64, 64), "icon.png");
        assert_eq!(p.width, Some(64));
        assert!(p.thumb.is_none());
    }

    #[test]
    fn animated_gif_preview_is_shrunk_and_still_animated() {
        let p = process(&gif(640, 480, 10), "dance.gif");
        assert!(p.animated);
        assert_eq!(p.width, Some(640));
        let thumb = p.thumb.expect("preview");
        let frames: Vec<_> = GifDecoder::new(Cursor::new(&thumb)).unwrap().into_frames().collect();
        assert_eq!(frames.len(), 10);
        let (w, h) = frames[0].as_ref().unwrap().buffer().dimensions();
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn long_gif_drops_frames() {
        let p = process(&gif(400, 100, 300), "long.gif");
        let thumb = p.thumb.expect("preview");
        let n = GifDecoder::new(Cursor::new(&thumb)).unwrap().into_frames().count();
        assert!(n <= MAX_GIF_FRAMES, "{n} frames");
    }

    #[test]
    fn other_files_are_just_files() {
        let p = process(b"hello world", "notes.txt");
        assert_eq!(p.mime, "text/plain");
        assert!(p.width.is_none() && p.thumb.is_none());
        // Something pretending to be a PNG is treated as a plain file.
        let p = process(b"\x89PNG\r\n\x1a\nnot really", "fake.png");
        assert!(p.width.is_none());
    }
}
