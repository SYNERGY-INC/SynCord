//! Update logic for pictures and files: uploads, previews, the viewer,
//! saving, GIF playback and favourite GIFs.

use std::sync::Arc;
use std::time::Instant;

use iced::Task;
use syncord_proto::{Attachment, MAX_ATTACHMENTS};

use crate::app::{App, Message, Overlay, PendingUpload, UploadState};
use crate::media::{self, Picture};
use crate::net;
use crate::settings::FavoriteGif;

/// Where an upload's bytes come from.
enum Source {
    File(std::path::PathBuf),
    Bytes(Vec<u8>),
}

/// Largest file the app will try to upload (the server may allow less).
const MAX_FILE: u64 = 100 * 1024 * 1024;

impl App {
    /// HTTP client, server base URL and login token, if logged in.
    pub(crate) fn http_ctx(&self) -> Option<(reqwest::Client, String, String)> {
        let token = self.settings.token.clone()?;
        Some((self.http.clone(), media::http_base(&net::server_url(&self.settings.server)), token))
    }

    /// Keys of the pictures in the channel being shown.
    pub(crate) fn visible_picture_keys(&self) -> Vec<String> {
        let Some(s) = &self.session else { return Vec::new() };
        let Some(st) = s.current.and_then(|c| s.chans.get(&c)) else { return Vec::new() };
        st.messages
            .iter()
            .flat_map(|m| &m.attachments)
            .filter(|a| a.is_image())
            .map(|a| media::key(&a.id, false))
            .collect()
    }

    /// Start loading previews for the channel being shown.
    pub(crate) fn request_pictures(&mut self) -> Task<Message> {
        let keys = self.visible_picture_keys();
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        let mut tasks = Vec::new();
        for key in keys {
            if self.images.start(&key) {
                let (http, base, token, id) = (http.clone(), base.clone(), token.clone(), key.clone());
                let k = key.clone();
                tasks.push(Task::perform(media::load(http, base, token, id, true), move |r| {
                    Message::PictureLoaded(k.clone(), r)
                }));
            }
        }
        Task::batch(tasks)
    }

    fn load_favorites(&mut self) -> Task<Message> {
        let ids: Vec<String> = self.settings.favorites_here().map(|f| f.attachment.id.clone()).collect();
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        Task::batch(ids.into_iter().filter(|id| self.images.start(id)).map(|id| {
            let k = id.clone();
            Task::perform(media::load(http.clone(), base.clone(), token.clone(), id, true), move |r| {
                Message::PictureLoaded(k.clone(), r)
            })
        }))
    }

    /// Whether GIFs need redrawing regularly right now.
    pub(crate) fn wants_animation(&self) -> bool {
        if self.settings_tab.is_some() {
            return false;
        }
        if let Some(Overlay::Image(a)) = &self.overlay {
            return a.animated;
        }
        if matches!(self.emoji_picker.as_ref().map(|p| p.tab), Some(crate::app::PickerTab::Gifs)) {
            return true;
        }
        let keys = self.visible_picture_keys();
        if self.settings.autoplay_gifs {
            self.images.has_animation(&keys)
        } else {
            // Only the GIF under the mouse plays.
            self.hovered_message_gif_keys().iter().any(|k| self.images.has_animation(std::slice::from_ref(k)))
        }
    }

    fn hovered_message_gif_keys(&self) -> Vec<String> {
        let (Some(s), Some(hovered)) = (&self.session, self.hovered) else { return Vec::new() };
        let Some(st) = s.current.and_then(|c| s.chans.get(&c)) else { return Vec::new() };
        st.messages
            .iter()
            .filter(|m| m.id == hovered)
            .flat_map(|m| &m.attachments)
            .filter(|a| a.animated)
            .map(|a| media::key(&a.id, false))
            .collect()
    }

    /// Milliseconds into the animation for a GIF in message `message_id`.
    pub(crate) fn animation_ms(&self, message_id: Option<syncord_proto::MessageId>) -> u64 {
        let playing = self.settings.autoplay_gifs || message_id.is_none() || self.hovered == message_id;
        if playing { self.now.saturating_duration_since(self.anim_epoch).as_millis() as u64 } else { 0 }
    }

    pub(crate) fn close_overlay(&mut self) {
        // The full-size picture is only kept while the viewer is open.
        if let Some(Overlay::Image(a)) = self.overlay.take() {
            self.images.remove(&media::key(&a.id, true));
        }
    }

    fn start_upload(&mut self, filename: String, source: Source) -> Task<Message> {
        let Some(channel) = self.session.as_ref().and_then(|s| s.current) else { return Task::none() };
        if self.pending.len() >= MAX_ATTACHMENTS {
            self.notice = Some(format!("You can attach up to {MAX_ATTACHMENTS} files to one message."));
            return Task::none();
        }
        let id = self.next_upload_id;
        self.next_upload_id += 1;
        self.pending.push(PendingUpload { id, filename: filename.clone(), state: UploadState::Uploading });
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        Task::perform(
            async move {
                let bytes = match source {
                    Source::Bytes(b) => b,
                    // Read in the background so big files don't freeze the window.
                    Source::File(path) => {
                        tokio::fs::read(&path).await.map_err(|e| format!("Couldn't read {filename}: {e}"))?
                    }
                };
                media::upload(http, base, token, channel, filename, bytes).await
            },
            move |r| Message::Uploaded(id, r),
        )
    }

    fn can_attach_here(&self) -> bool {
        self.session.as_ref().and_then(|s| s.current_info()).is_some() && self.settings_tab.is_none()
    }

    pub(crate) fn update_media(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AttachFiles => {
                if !self.can_attach_here() {
                    return Task::none();
                }
                return Task::perform(
                    async {
                        let files = rfd::AsyncFileDialog::new().set_title("Attach files").pick_files().await;
                        files.unwrap_or_default().into_iter().map(|f| f.path().to_path_buf()).collect()
                    },
                    Message::FilesChosen,
                );
            }
            Message::FilesChosen(paths) => {
                if !self.can_attach_here() {
                    return Task::none();
                }
                let mut tasks = Vec::new();
                for path in paths {
                    let name =
                        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                    if path.is_dir() {
                        self.notice = Some(format!("{name} is a folder. Zip it first to send it."));
                        continue;
                    }
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE) {
                        self.notice = Some(format!("{name} is bigger than 100 MB."));
                        continue;
                    }
                    tasks.push(self.start_upload(name, Source::File(path)));
                }
                return Task::batch(tasks);
            }
            Message::PasteShortcut => {
                if !self.can_attach_here() {
                    return Task::none();
                }
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(|| {
                            let img = arboard::Clipboard::new().ok()?.get_image().ok()?;
                            let rgba = image::RgbaImage::from_raw(
                                img.width as u32,
                                img.height as u32,
                                img.bytes.into_owned(),
                            )?;
                            let mut png = std::io::Cursor::new(Vec::new());
                            rgba.write_to(&mut png, image::ImageFormat::Png).ok()?;
                            Some(png.into_inner())
                        })
                        .await
                        .ok()
                        .flatten()
                    },
                    Message::Pasted,
                );
            }
            Message::Pasted(Some(png)) => {
                return self.start_upload("pasted-image.png".into(), Source::Bytes(png));
            }
            Message::Pasted(None) => {} // plain text paste: the text box handles it
            Message::Uploaded(id, result) => {
                if self.pending_channel != self.session.as_ref().and_then(|s| s.current) {
                    return Task::none();
                }
                if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
                    p.state = match result {
                        Ok(a) => UploadState::Ready(a),
                        Err(e) => UploadState::Failed(e),
                    };
                }
            }
            Message::RemovePending(id) => self.pending.retain(|p| p.id != id),
            Message::PictureLoaded(key, result) => {
                if let Err(e) = &result {
                    eprintln!("couldn't load picture {key}: {e}");
                }
                self.images.finish(key, result);
            }
            Message::OpenViewer(a) => {
                self.overlay = Some(Overlay::Image(a.clone()));
                let key = media::key(&a.id, true);
                if self.images.start(&key)
                    && let Some((http, base, token)) = self.http_ctx()
                {
                    // Animated GIFs keep every frame in memory, so they're shown smaller.
                    let side = if a.animated { 640 } else { media::VIEWER_SIDE };
                    let id = a.id.clone();
                    return Task::perform(
                        async move {
                            let bytes = media::download(http, base, token, id, false).await?;
                            tokio::task::spawn_blocking(move || media::decode(&bytes, side))
                                .await
                                .map_err(|e| e.to_string())?
                                .map(Arc::new)
                        },
                        move |r| Message::PictureLoaded(key.clone(), r),
                    );
                }
            }
            Message::SaveAttachment(a) => {
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                return Task::perform(
                    async move {
                        let Some(file) = rfd::AsyncFileDialog::new().set_file_name(&a.filename).save_file().await
                        else {
                            return Ok(None);
                        };
                        let bytes = media::download(http, base, token, a.id, false).await?;
                        tokio::fs::write(file.path(), bytes).await.map_err(|e| format!("Couldn't save: {e}"))?;
                        Ok(Some(file.path().display().to_string()))
                    },
                    Message::Saved,
                );
            }
            Message::Saved(Ok(Some(path))) => self.notice = Some(format!("Saved to {path}")),
            Message::Saved(Ok(None)) => {}
            Message::Saved(Err(e)) => self.notice = Some(e),
            Message::ToggleFavoriteGif(a) => {
                let server = self.settings.server.clone();
                let before = self.settings.favorite_gifs.len();
                self.settings.favorite_gifs.retain(|f| !(f.server == server && f.attachment.id == a.id));
                if self.settings.favorite_gifs.len() == before {
                    self.settings.favorite_gifs.insert(0, FavoriteGif { server, attachment: a });
                    self.notice = Some("Added to your favourite GIFs (in the emoji picker).".into());
                }
                self.settings.save();
            }
            Message::SendFavorite(a) => {
                self.emoji_picker = None;
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                let Some(channel) = self.session.as_ref().and_then(|s| s.current) else { return Task::none() };
                // Copy the GIF into this conversation, then send it straight away.
                let id = self.next_upload_id;
                self.next_upload_id += 1;
                self.pending.push(PendingUpload { id, filename: a.filename.clone(), state: UploadState::Uploading });
                return Task::perform(
                    async move {
                        let bytes = media::download(http.clone(), base.clone(), token.clone(), a.id, false).await?;
                        media::upload(http, base, token, channel, a.filename, bytes).await
                    },
                    move |r| Message::Uploaded(id, r),
                )
                .chain(Task::done(Message::SendMessage));
            }
            Message::AnimationTick => self.now = Instant::now(),
            Message::ToggleAutoplayGifs(v) => {
                self.settings.autoplay_gifs = v;
                self.settings.save();
            }
            _ => {}
        }
        if matches!(self.emoji_picker.as_ref().map(|p| p.tab), Some(crate::app::PickerTab::Gifs)) {
            return self.load_favorites();
        }
        Task::none()
    }

    /// The decoded preview for an attachment, if loaded.
    pub(crate) fn preview(&self, a: &Attachment) -> Option<&Arc<Picture>> {
        self.images.picture(&media::key(&a.id, false))
    }

    pub(crate) fn picture_failed(&self, a: &Attachment) -> bool {
        matches!(self.images.get(&media::key(&a.id, false)), Some(media::Entry::Failed))
    }
}
