//! Cropping a new profile picture or banner, like Discord's "Edit image":
//! drag to move, zoom with the slider or mouse wheel, and the result is cut to
//! the shape the server expects (square avatars, 5:2 banners). GIFs stay animated.

use std::io::Cursor;
use std::sync::Arc;

use iced::widget::image::Handle;
use iced::widget::{button, center, column, container, image as picture, mouse_area, row, slider, text};
use iced::{Alignment, Border, Color, Element, Length, Point, Task};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::imageops::FilterType;
use image::{AnimationDecoder, Delay, Frame, ImageFormat, RgbaImage};
use celerity_proto::{AVATAR_SIZE, BANNER_SIZE, PROFILE_IMAGE_PATH};

use crate::app::{App, Message};
use crate::style::{self, bold, card};

/// Most frames kept from an animated picture (GIFs are cut short after this).
const MAX_FRAMES: usize = 100;
/// The upload limit for profile pictures, kept in step with the server.
const MAX_BYTES: usize = 12 * 1024 * 1024;
/// On-screen size of the crop preview.
const PREVIEW_AVATAR: f32 = 240.0;
const PREVIEW_BANNER: (f32, f32) = (420.0, 168.0);

/// A decoded picture to crop: its frames (with delays in ms), shrunk to a workable size.
#[derive(Debug)]
pub struct Source {
    pub frames: Vec<(RgbaImage, u32)>,
}

impl Source {
    pub fn size(&self) -> (u32, u32) {
        self.frames[0].0.dimensions()
    }

    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
}

#[derive(Debug, Clone)]
pub struct CropState {
    pub banner: bool,
    pub source: Arc<Source>,
    /// 1 shows as much as fits; up to 4 zooms in.
    pub zoom: f32,
    /// Middle of the crop, as a fraction of the picture's width and height.
    pub center: (f32, f32),
    /// Where a drag started (pointer, and the middle at that time).
    pub drag: Option<(Point, (f32, f32))>,
    pub last_pointer: Option<Point>,
    pub preview: Handle,
    pub busy: bool,
    pub error: Option<String>,
}

fn aspect(banner: bool) -> f32 {
    if banner { BANNER_SIZE.0 as f32 / BANNER_SIZE.1 as f32 } else { 1.0 }
}

fn preview_size(banner: bool) -> (f32, f32) {
    if banner { PREVIEW_BANNER } else { (PREVIEW_AVATAR, PREVIEW_AVATAR) }
}

fn output_size(banner: bool) -> (u32, u32) {
    if banner { BANNER_SIZE } else { (AVATAR_SIZE, AVATAR_SIZE) }
}

/// The crop rectangle (x, y, w, h) in source pixels.
pub fn crop_rect(src: (u32, u32), banner: bool, zoom: f32, center: (f32, f32)) -> (u32, u32, u32, u32) {
    let (w, h) = (src.0 as f32, src.1 as f32);
    let a = aspect(banner);
    // The biggest rectangle of the right shape that fits, then zoomed in.
    let base_w = w.min(h * a);
    let cw = (base_w / zoom.clamp(1.0, 4.0)).max(1.0);
    let ch = (cw / a).max(1.0);
    let cx = (center.0 * w).clamp(cw / 2.0, w - cw / 2.0);
    let cy = (center.1 * h).clamp(ch / 2.0, h - ch / 2.0);
    let x = (cx - cw / 2.0).round().max(0.0) as u32;
    let y = (cy - ch / 2.0).round().max(0.0) as u32;
    (x, y, (cw.round() as u32).min(src.0 - x).max(1), (ch.round() as u32).min(src.1 - y).max(1))
}

/// Keep the middle where the crop can actually be.
fn clamp_center(src: (u32, u32), banner: bool, zoom: f32, center: (f32, f32)) -> (f32, f32) {
    let (x, y, w, h) = crop_rect(src, banner, zoom, center);
    ((x as f32 + w as f32 / 2.0) / src.0 as f32, (y as f32 + h as f32 / 2.0) / src.1 as f32)
}

fn cut(img: &RgbaImage, rect: (u32, u32, u32, u32), out: (u32, u32), filter: FilterType) -> RgbaImage {
    let (x, y, w, h) = rect;
    let part = image::imageops::crop_imm(img, x, y, w, h).to_image();
    image::imageops::resize(&part, out.0, out.1, filter)
}

/// Decode a picture for cropping (in the background: big files take a moment).
pub fn load(bytes: &[u8], banner: bool) -> Result<Source, String> {
    let format = image::guess_format(bytes).map_err(|_| "That isn't a picture Celerity can read.".to_string())?;
    // Enough detail for the output, without holding huge frames in memory.
    let (still_max, anim_max) = if banner { (1600, 720) } else { (1024, 384) };
    let shrink = |img: RgbaImage, max: u32| {
        let (w, h) = img.dimensions();
        if w.max(h) <= max {
            img
        } else {
            let s = max as f32 / w.max(h) as f32;
            image::imageops::resize(
                &img,
                ((w as f32 * s) as u32).max(1),
                ((h as f32 * s) as u32).max(1),
                FilterType::Triangle,
            )
        }
    };
    if format == ImageFormat::Gif {
        let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("That GIF is damaged: {e}"))?;
        let mut frames = Vec::new();
        for f in decoder.into_frames().take(MAX_FRAMES) {
            let Ok(f) = f else { break };
            let (n, d) = f.delay().numer_denom_ms();
            let delay = (n / d.max(1)).max(20);
            frames.push((shrink(f.into_buffer(), anim_max), delay));
        }
        if frames.is_empty() {
            return Err("That GIF has no pictures in it.".into());
        }
        if frames.len() == 1 {
            let (img, _) = frames.pop().unwrap();
            return Ok(Source { frames: vec![(img, 0)] });
        }
        return Ok(Source { frames });
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("Couldn't read that picture: {e}"))?;
    Ok(Source { frames: vec![(shrink(img.to_rgba8(), still_max), 0)] })
}

/// Cut every frame and encode the result for upload.
pub fn render(source: &Source, banner: bool, zoom: f32, center: (f32, f32)) -> Result<Vec<u8>, String> {
    let rect = crop_rect(source.size(), banner, zoom, center);
    let out_size = output_size(banner);
    let mut out = Vec::new();
    if source.animated() {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(Repeat::Infinite).map_err(|e| e.to_string())?;
        for (img, delay) in &source.frames {
            let frame = Frame::from_parts(
                cut(img, rect, out_size, FilterType::Triangle),
                0,
                0,
                Delay::from_numer_denom_ms(*delay, 1),
            );
            enc.encode_frame(frame).map_err(|e| format!("Couldn't make the GIF: {e}"))?;
        }
        drop(enc);
    } else {
        let img = cut(&source.frames[0].0, rect, out_size, FilterType::Lanczos3);
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).map_err(|e| e.to_string())?;
    }
    if out.len() > MAX_BYTES {
        return Err("That GIF is too big after cropping. Try a shorter one.".into());
    }
    Ok(out)
}

fn preview(source: &Source, banner: bool, zoom: f32, center: (f32, f32)) -> Handle {
    let (pw, ph) = preview_size(banner);
    let rect = crop_rect(source.size(), banner, zoom, center);
    let img = cut(&source.frames[0].0, rect, ((pw * 1.5) as u32, (ph * 1.5) as u32), FilterType::Triangle);
    let (w, h) = img.dimensions();
    Handle::from_rgba(w, h, img.into_raw())
}

impl CropState {
    pub fn new(source: Source, banner: bool) -> Self {
        let source = Arc::new(source);
        let preview = preview(&source, banner, 1.0, (0.5, 0.5));
        Self {
            banner,
            source,
            zoom: 1.0,
            center: (0.5, 0.5),
            drag: None,
            last_pointer: None,
            preview,
            busy: false,
            error: None,
        }
    }

    fn refresh(&mut self) {
        self.center = clamp_center(self.source.size(), self.banner, self.zoom, self.center);
        self.preview = preview(&self.source, self.banner, self.zoom, self.center);
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(1.0, 4.0);
        self.refresh();
    }

    /// The pointer moved over the preview; while dragging, the picture follows it.
    pub fn pointer(&mut self, p: Point) {
        self.last_pointer = Some(p);
        let Some((start, from)) = self.drag else { return };
        let (pw, ph) = preview_size(self.banner);
        let (_, _, cw, ch) = crop_rect(self.source.size(), self.banner, self.zoom, from);
        let (sw, sh) = self.source.size();
        // Moving the picture right shows more of its left side.
        let dx = (p.x - start.x) / pw * cw as f32 / sw as f32;
        let dy = (p.y - start.y) / ph * ch as f32 / sh as f32;
        self.center = (from.0 - dx, from.1 - dy);
        self.refresh();
    }
}

impl App {
    pub(crate) fn update_crop(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PickProfileImage(banner) => {
                return Task::perform(
                    async move {
                        let file = rfd::AsyncFileDialog::new()
                            .set_title(if banner { "Choose a banner" } else { "Choose a profile picture" })
                            .add_filter("Pictures", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                            .pick_file()
                            .await?;
                        let bytes = file.read().await;
                        Some(
                            tokio::task::spawn_blocking(move || load(&bytes, banner))
                                .await
                                .map_err(|e| e.to_string())
                                .and_then(|r| r),
                        )
                    },
                    move |r| Message::CropLoaded(banner, r.map(|r| r.map(Arc::new))),
                );
            }
            Message::CropLoaded(_, None) => {}
            Message::CropLoaded(banner, Some(Ok(source))) => {
                let source = Arc::try_unwrap(source).unwrap_or_else(|a| Source { frames: a.frames.clone() });
                self.crop = Some(CropState::new(source, banner));
                self.overlay = Some(crate::app::Overlay::Crop);
            }
            Message::CropLoaded(_, Some(Err(e))) => self.notice = Some(e),
            Message::CropZoom(z) => {
                if let Some(c) = &mut self.crop {
                    c.set_zoom(z);
                }
            }
            Message::CropScroll(lines) => {
                if let Some(c) = &mut self.crop {
                    let z = c.zoom * (1.0 + lines * 0.1);
                    c.set_zoom(z);
                }
            }
            Message::CropPointer(p) => {
                if let Some(c) = &mut self.crop {
                    c.pointer(p);
                }
            }
            Message::CropDrag(on) => {
                if let Some(c) = &mut self.crop {
                    c.drag = if on { c.last_pointer.map(|p| (p, c.center)) } else { None };
                }
            }
            Message::CropApply => {
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                let Some(c) = &mut self.crop else { return Task::none() };
                c.busy = true;
                c.error = None;
                let (source, banner, zoom, center) = (c.source.clone(), c.banner, c.zoom, c.center);
                return Task::perform(
                    async move {
                        let bytes = tokio::task::spawn_blocking(move || render(&source, banner, zoom, center))
                            .await
                            .map_err(|e| e.to_string())??;
                        let url =
                            format!("{base}{PROFILE_IMAGE_PATH}?kind={}", if banner { "banner" } else { "avatar" });
                        let resp = http
                            .post(url)
                            .bearer_auth(token)
                            .body(bytes)
                            .send()
                            .await
                            .map_err(|e| format!("Upload failed: {e}"))?;
                        if resp.status().is_success() {
                            Ok(())
                        } else if resp.status().as_u16() == 404 {
                            Err("The server needs updating before you can upload profile pictures.".into())
                        } else {
                            Err(resp.text().await.unwrap_or_else(|_| "Upload failed".into()))
                        }
                    },
                    Message::CropDone,
                );
            }
            Message::CropDone(Ok(())) => {
                self.crop = None;
                self.close_overlay();
            }
            Message::CropDone(Err(e)) => {
                if let Some(c) = &mut self.crop {
                    c.busy = false;
                    c.error = Some(e);
                }
            }
            Message::CropCancel => {
                self.crop = None;
                self.close_overlay();
            }
            Message::RemoveProfileImage(banner) => self.send(celerity_proto::ClientMsg::ClearProfileImage { banner }),
            _ => {}
        }
        Task::none()
    }

    /// Upload/remove buttons for the profile picture and banner, in profile settings.
    pub(crate) fn picture_buttons<'a>(&'a self, me: &celerity_proto::User) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let line = |name: &'static str, hint: &'static str, banner: bool, has: bool| {
            let mut r = row![
                button(text(if has { "Change" } else { "Upload" }).size(fs * 0.9))
                    .padding([7, 14])
                    .on_press(Message::PickProfileImage(banner))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if has {
                r = r.push(
                    button(text("Remove").size(fs * 0.9))
                        .padding([7, 10])
                        .on_press(Message::RemoveProfileImage(banner))
                        .style(style::link_button(t.text.0, t.header.0)),
                );
            }
            column![style::label(t, name, fs), r, text(hint).size(fs * 0.75).color(t.muted.0)].spacing(6)
        };
        row![
            line("Profile picture", "PNG, JPEG or GIF (GIFs move)", false, me.profile.avatar.is_some()),
            line("Banner", "Shown on your profile card; GIFs move too", true, me.profile.banner.is_some()),
        ]
        .spacing(32)
        .into()
    }

    pub(crate) fn crop_dialog(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(c) = &self.crop else { return column![].into() };
        let (pw, ph) = preview_size(c.banner);
        let radius = if c.banner { t.radius } else { pw / 2.0 };
        let picture = mouse_area(picture(c.preview.clone()).width(pw).height(ph).border_radius(radius))
            .on_move(Message::CropPointer)
            .on_press(Message::CropDrag(true))
            .on_release(Message::CropDrag(false))
            .on_exit(Message::CropDrag(false))
            .on_scroll(|d| {
                let lines = match d {
                    iced::mouse::ScrollDelta::Lines { y, .. } => y,
                    iced::mouse::ScrollDelta::Pixels { y, .. } => y / 40.0,
                };
                Message::CropScroll(lines)
            })
            .interaction(iced::mouse::Interaction::Grab);
        let title = if c.banner { "Edit banner" } else { "Edit profile picture" };
        let mut body = column![
            text(title).size(fs * 1.3).font(bold()).color(t.header.0),
            center(picture).height(ph + 24.0).style(move |_| container::Style {
                background: Some(Color::from_rgb8(0x10, 0x11, 0x14).into()),
                border: Border { radius: 8.0.into(), ..Default::default() },
                ..Default::default()
            }),
            row![
                text("Zoom").size(fs * 0.85).color(t.muted.0),
                slider(1.0..=4.0, c.zoom, Message::CropZoom).step(0.01_f32),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            text(if c.source.animated() {
                format!("Drag to move it. It stays animated ({} frames).", c.source.frames.len())
            } else {
                "Drag to move it, scroll or use the slider to zoom.".to_string()
            })
            .size(fs * 0.8)
            .color(t.muted.0),
        ]
        .spacing(14);
        if let Some(e) = &c.error {
            body = body.push(text(e.clone()).size(fs * 0.85).color(t.danger.0));
        }
        body = body.push(
            row![
                iced::widget::space::horizontal(),
                button(text("Cancel").size(fs * 0.95))
                    .padding([8, 16])
                    .on_press(Message::CropCancel)
                    .style(style::link_button(t.text.0, t.header.0)),
                button(text(if c.busy { "Saving..." } else { "Apply" }).size(fs * 0.95))
                    .padding([8, 18])
                    .on_press_maybe((!c.busy).then_some(Message::CropApply))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        container(body)
            .width(Length::Fixed(pw.max(300.0) + 80.0))
            .padding(22)
            .style(card(t.chat.0, t.radius * 1.5))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_shapes_and_limits() {
        // A wide picture: the avatar crop is a centred square, as tall as the picture.
        assert_eq!(crop_rect((400, 200), false, 1.0, (0.5, 0.5)), (100, 0, 200, 200));
        // Zoomed in twice: half the size.
        assert_eq!(crop_rect((400, 200), false, 2.0, (0.5, 0.5)), (150, 50, 100, 100));
        // Dragged past the edge: stops at the edge.
        assert_eq!(crop_rect((400, 200), false, 1.0, (0.0, 0.5)), (0, 0, 200, 200));
        // Banners are 5:2.
        let (_, _, w, h) = crop_rect((1000, 1000), true, 1.0, (0.5, 0.5));
        assert_eq!((w, h), (1000, 400));
    }

    #[test]
    fn renders_stills_and_gifs_at_the_right_size() {
        let src = Source { frames: vec![(RgbaImage::from_pixel(300, 200, image::Rgba([9, 9, 9, 255])), 0)] };
        let png = render(&src, false, 1.0, (0.5, 0.5)).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (AVATAR_SIZE, AVATAR_SIZE));

        let frames = (0..3).map(|i| (RgbaImage::from_pixel(720, 300, image::Rgba([i * 60, 0, 0, 255])), 80)).collect();
        let gif = render(&Source { frames }, true, 1.5, (0.3, 0.5)).unwrap();
        let decoder = GifDecoder::new(Cursor::new(&gif)).unwrap();
        let frames: Vec<_> = decoder.into_frames().collect::<Result<_, _>>().unwrap();
        assert_eq!(frames.len(), 3, "still animated");
        assert_eq!(frames[0].buffer().dimensions(), BANNER_SIZE);
    }
}
