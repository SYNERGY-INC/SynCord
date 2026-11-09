//! Videos in the chat: a preview with a play button; press it and the video is
//! downloaded (with progress) and plays in place, with play/pause, a seek bar and
//! volume. One video plays at a time.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use celerity_proto::Attachment;
use celerity_video::Player;
use iced::widget::image::Handle;
use iced::widget::{button, center, column, container, image, row, slider, space, stack, text};
use iced::{Alignment, Border, Color, ContentFit, Element, Length, Task};

use crate::app::{App, Message};
use crate::icons::{self, icon};
use crate::media::{self, format_size};
use crate::style::{self, bold, card};

const VIDEO_W: f32 = 400.0;
const VIDEO_H: f32 = 300.0;
/// Frames are shrunk to this (the video is shown at most 400 px wide; 2x for sharp screens).
const FRAME_SIDE: u32 = 800;

/// A player, passed around in messages.
#[derive(Clone)]
pub struct PlayerHandle(pub Arc<Player>);

impl std::fmt::Debug for PlayerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Player")
    }
}

/// The video that's open (playing or paused).
pub struct ActiveVideo {
    pub id: String,
    pub player: Arc<Player>,
    pub frame: Option<Handle>,
    serial: u64,
    /// Keep picking up frames until then (after seeking while paused).
    refresh_until: std::time::Instant,
}

/// Downloaded videos live here; only the one being watched is kept.
fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("celerity").join("videos")
}

/// Forget videos from earlier runs (they can be big).
pub fn clear_cache() {
    let _ = std::fs::remove_dir_all(cache_dir());
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl App {
    /// Redraw while a video plays or downloads.
    pub(crate) fn video_busy(&self) -> bool {
        !self.video_loading.is_empty()
            || self.video.as_ref().is_some_and(|v| v.player.is_playing() || v.refresh_until > std::time::Instant::now())
    }

    /// Stop and forget the open video (when leaving the channel, for example).
    pub(crate) fn stop_video(&mut self) {
        if let Some(v) = self.video.take() {
            v.player.pause();
            let _ = std::fs::remove_file(cache_dir().join(format!("{}.mp4", v.id)));
        }
    }

    pub(crate) fn update_video(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PlayVideo(a) => {
                if let Some(v) = &self.video
                    && v.id == a.id
                {
                    if v.player.is_playing() {
                        v.player.pause();
                    } else {
                        v.player.play();
                    }
                    return Task::none();
                }
                if self.video_loading.contains_key(&a.id) {
                    return Task::none();
                }
                self.stop_video();
                self.video_errors.remove(&a.id);
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                let done = Arc::new(AtomicU64::new(0));
                self.video_loading.insert(a.id.clone(), (done.clone(), a.size));
                let id = a.id.clone();
                return Task::perform(
                    async move {
                        let dir = cache_dir();
                        tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
                        let path = dir.join(format!("{}.mp4", a.id));
                        if tokio::fs::metadata(&path).await.map(|m| m.len()).ok() != Some(a.size) {
                            media::save(http, base, token, a.id.clone(), path.clone(), done).await?;
                        }
                        let player = tokio::task::spawn_blocking(move || Player::open(&path, FRAME_SIDE))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok(Arc::new(player))
                    },
                    move |r| Message::VideoReady(id.clone(), r.map(PlayerHandle)),
                );
            }
            Message::VideoReady(id, result) => {
                self.video_loading.remove(&id);
                match result {
                    Ok(PlayerHandle(player)) => {
                        self.stop_video();
                        player.set_volume(if self.settings.video_muted { 0.0 } else { self.settings.video_volume });
                        player.play();
                        let frame = player.frame().map(|f| Handle::from_rgba(f.width, f.height, f.rgba.to_vec()));
                        self.video = Some(ActiveVideo {
                            id,
                            player,
                            frame,
                            serial: 0,
                            refresh_until: std::time::Instant::now(),
                        });
                    }
                    Err(e) => {
                        self.video_errors.insert(id, e);
                    }
                }
            }
            Message::VideoTick => {
                if let Some(v) = &mut self.video
                    && let Some(f) = v.player.frame()
                    && f.serial != v.serial
                {
                    v.serial = f.serial;
                    v.frame = Some(Handle::from_rgba(f.width, f.height, f.rgba.to_vec()));
                }
            }
            Message::VideoSeek(ms) => {
                if let Some(v) = &mut self.video {
                    v.player.seek(ms as u64);
                    // The new frame takes a moment to decode; show it even if paused.
                    v.refresh_until = std::time::Instant::now() + std::time::Duration::from_secs(2);
                }
            }
            Message::VideoVolume(vol) => {
                self.settings.video_volume = vol;
                self.settings.video_muted = false;
                if let Some(v) = &self.video {
                    v.player.set_volume(vol);
                }
                self.settings.save();
            }
            Message::VideoMute => {
                self.settings.video_muted = !self.settings.video_muted;
                if let Some(v) = &self.video {
                    v.player.set_volume(if self.settings.video_muted { 0.0 } else { self.settings.video_volume });
                }
                self.settings.save();
            }
            _ => {}
        }
        Task::none()
    }

    /// A video attachment in the chat.
    pub(crate) fn video_view<'a>(&'a self, a: &'a Attachment) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let (w, h) = media::fit(a.width.unwrap_or(16), a.height.unwrap_or(9), VIDEO_W, VIDEO_H);
        let active = self.video.as_ref().filter(|v| v.id == a.id);
        let duration = a.video.map(|v| v.duration_ms).unwrap_or(0);

        let picture: Element<'a, Message> = match (active.and_then(|v| v.frame.clone()), self.preview(a)) {
            (Some(frame), _) => image(frame).width(w).height(h).content_fit(ContentFit::Contain).into(),
            (None, Some(p)) => image(p.frame(0).clone()).width(w).height(h).content_fit(ContentFit::Contain).into(),
            (None, None) => container(space()).width(w).height(h).style(style::bg(Color::BLACK)).into(),
        };
        let playing = active.is_some_and(|v| v.player.is_playing());
        let mut layers = stack![container(picture).width(w).height(h).style(style::bg(Color::BLACK))];
        if !playing {
            // The big round play button over the picture.
            let label: Element<'a, Message> = match self.video_loading.get(&a.id) {
                Some((done, total)) => {
                    let pct = (done.load(Ordering::Relaxed) * 100).checked_div(*total).unwrap_or(0).min(100);
                    text(format!("{pct}%")).size(fs * 0.9).font(bold()).color(Color::WHITE).into()
                }
                None => icon(&icons::PLAY, 30.0, Color::WHITE),
            };
            let play = button(center(label).width(64).height(64))
                .padding(0)
                .on_press(Message::PlayVideo(a.clone()))
                .style(|_, status| button::Style {
                    background: Some(
                        Color::from_rgba(
                            0.0,
                            0.0,
                            0.0,
                            if matches!(status, button::Status::Hovered) { 0.8 } else { 0.6 },
                        )
                        .into(),
                    ),
                    border: Border { radius: 32.0.into(), ..Default::default() },
                    ..Default::default()
                });
            layers = layers.push(center(play).width(w).height(h));
            if active.is_none() && duration > 0 {
                let badge = container(text(clock(duration)).size(fs * 0.75).color(Color::WHITE))
                    .padding([2, 6])
                    .style(style::flat(Color::from_rgba(0.0, 0.0, 0.0, 0.65), 4.0));
                layers = layers.push(container(badge).width(w).height(h).padding(8).align_right(w).align_bottom(h));
            }
        } else {
            // Click the picture to pause.
            layers = layers.push(
                button(space().width(w).height(h))
                    .padding(0)
                    .on_press(Message::PlayVideo(a.clone()))
                    .style(|_, _| button::Style::default()),
            );
        }

        let header = row![
            text(crate::view_extras::middle_ellipsis(&a.filename, 34))
                .size(fs * 0.8)
                .color(t.muted.0)
                .width(Length::Fill),
            text(format_size(a.size)).size(fs * 0.75).color(t.muted.0),
            button(icon(&icons::DOWNLOAD, 16.0, t.text.0))
                .padding(3)
                .on_press(Message::SaveAttachment(a.clone()))
                .style(style::icon_button(t)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .width(w);
        let mut col = column![header, layers].spacing(4);

        if let Some(v) = active {
            let pos = v.player.position_ms().min(duration.max(1));
            let muted = self.settings.video_muted;
            let controls = row![
                button(icon(if playing { &icons::PAUSE } else { &icons::PLAY }, 16.0, t.header.0))
                    .padding(4)
                    .on_press(Message::PlayVideo(a.clone()))
                    .style(style::icon_button(t)),
                text(format!("{} / {}", clock(pos), clock(duration))).size(fs * 0.75).color(t.text.0),
                slider(0.0..=duration.max(1) as f32, pos as f32, Message::VideoSeek).width(Length::Fill),
                button(icon(if muted { &icons::VOLUME_OFF } else { &icons::VOLUME }, 16.0, t.header.0))
                    .padding(4)
                    .on_press(Message::VideoMute)
                    .style(style::icon_button(t)),
                slider(0.0..=1.0, if muted { 0.0 } else { self.settings.video_volume }, Message::VideoVolume)
                    .step(0.01_f32)
                    .width(70),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(w);
            col = col.push(container(controls).padding([2, 4]).style(card(t.floating.0, 6.0)));
        }
        if let Some(e) = self.video_errors.get(&a.id) {
            col = col.push(text(format!("Can't play this video here: {e}")).size(fs * 0.8).color(t.danger.0));
        }
        col.into()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clock() {
        assert_eq!(super::clock(0), "0:00");
        assert_eq!(super::clock(65_400), "1:05");
        assert_eq!(super::clock(3_725_000), "1:02:05");
    }
}
