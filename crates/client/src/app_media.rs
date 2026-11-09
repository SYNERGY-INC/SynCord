//! Update logic for pictures and files: uploads, previews, the viewer,
//! saving, GIF playback and favourite GIFs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use celerity_proto::{Attachment, MAX_ATTACHMENTS};
use iced::Task;

use crate::app::{App, Clip, Download, Message, Overlay, PendingUpload, UploadState};
use crate::media::{self, Picture, Source};
use crate::settings::FavoriteGif;

/// Largest file the app will try to upload (the server may allow less).
const MAX_FILE_MB: u64 = 1024;
const OLD_SERVER_LIMIT_MB: u64 = 25;

/// "1 GB" or "250 MB".
fn limit_text(mb: u64) -> String {
    if mb >= 1024 && mb.is_multiple_of(1024) { format!("{} GB", mb / 1024) } else { format!("{mb} MB") }
}

impl App {
    /// HTTP client, server base URL and login token, if logged in.
    pub(crate) fn http_ctx(&self) -> Option<(reqwest::Client, String, String)> {
        let token = self.settings.token.clone()?;
        Some((self.http.clone(), self.server_base(), token))
    }

    /// Start loading previews for the channel being shown. GIFs wait until they're on
    /// screen (see `PictureSeen`): each one keeps all its frames in memory.
    pub(crate) fn request_pictures(&mut self) -> Task<Message> {
        let keys = self.still_picture_keys();
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        // Server emoji and the stickers in the messages.
        let mut tasks = vec![self.request_expressions()];
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

    /// Pictures in the open channel that aren't GIFs (cheap to load up front).
    fn still_picture_keys(&self) -> Vec<String> {
        let Some(s) = &self.session else { return Vec::new() };
        // The open channel, and the open thread's messages beside it.
        let shown = [s.current, s.open_thread].into_iter().flatten().filter_map(|c| s.chans.get(&c));
        shown
            .flat_map(|st| st.messages.iter())
            .flat_map(|m| m.attachments.iter().chain(m.embeds.iter().filter_map(|e| e.image.as_ref())))
            .filter(|a| (a.is_image() && !a.animated) || a.is_video())
            .map(|a| media::key(&a.id, false))
            .collect()
    }

    /// Load one chat picture preview, unless it's loaded or on its way.
    fn load_preview(&mut self, key: String) -> Task<Message> {
        if !self.images.start(&key) {
            return Task::none();
        }
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        let k = key.clone();
        Task::perform(media::load(http, base, token, key, true), move |r| Message::PictureLoaded(k.clone(), r))
    }

    /// Whether GIFs need redrawing regularly right now.
    pub(crate) fn wants_animation(&self) -> bool {
        if self.settings_tab.is_some() {
            return false;
        }
        if let Some(Overlay::Image(a)) = &self.overlay {
            return a.animated;
        }
        if let Some(Overlay::Profile(id)) = &self.overlay
            && let Some(u) = self.session.as_ref().and_then(|s| s.users.get(id))
        {
            let keys: Vec<String> = [
                u.profile.avatar.as_ref().filter(|a| a.animated).map(|a| media::avatar_key(&a.id, true)),
                u.profile.banner.as_ref().map(|b| media::banner_key(&b.id)),
            ]
            .into_iter()
            .flatten()
            .collect();
            if self.images.has_animation(&keys) {
                return true;
            }
        }
        if matches!(self.emoji_picker.as_ref().map(|p| p.tab), Some(crate::app::PickerTab::Gifs)) {
            // Favourites play, and so does the search result under the mouse.
            let mut keys: Vec<String> = self.settings.favorites_here().map(|f| f.attachment.id.clone()).collect();
            keys.extend(self.gifs.hovered.as_deref().map(|id| media::gif_key(id, true)));
            return self.images.has_animation(&keys);
        }
        // GIFs in the chat play by themselves (see `widgets::picture`).
        false
    }

    /// Milliseconds into the animation for a GIF in message `message_id`.
    pub(crate) fn animation_ms(&self, message_id: Option<celerity_proto::MessageId>) -> u64 {
        let playing = self.settings.autoplay_gifs || message_id.is_none() || self.hovered == message_id;
        if playing { self.now.saturating_duration_since(self.anim_epoch).as_millis() as u64 } else { 0 }
    }

    pub(crate) fn close_overlay(&mut self) {
        // The full-size picture is only kept while the viewer is open.
        match self.overlay.take() {
            Some(Overlay::Image(a)) => self.images.remove(&media::key(&a.id, true)),
            Some(Overlay::Crop) => self.crop = None,
            Some(Overlay::Profile(id)) => {
                if let Some(u) = self.session.as_ref().and_then(|s| s.users.get(&id)).cloned() {
                    self.drop_profile_extras(&u);
                }
            }
            _ => {}
        }
        self.copying = None;
    }

    /// The upload limit: the server's, and never more than the app's.
    fn upload_limit_mb(&self) -> u64 {
        match self.session.as_ref().map(|s| s.max_upload_mb) {
            Some(mb) if mb > 0 => mb.min(MAX_FILE_MB),
            // Servers older than 0.4 don't say; they allowed 25 MB unless changed.
            _ => OLD_SERVER_LIMIT_MB,
        }
    }

    fn start_upload(&mut self, filename: String, source: Source, size: u64) -> Task<Message> {
        let Some(channel) = self.session.as_ref().and_then(|s| s.current) else { return Task::none() };
        if self.pending.len() >= MAX_ATTACHMENTS {
            self.notice = Some(format!("You can attach up to {MAX_ATTACHMENTS} files to one message."));
            return Task::none();
        }
        let limit = self.upload_limit_mb();
        if size > limit * 1024 * 1024 {
            let mut notice = format!("{filename} is too big: files can be up to {} here.", limit_text(limit));
            if self.session.as_ref().is_some_and(|s| s.max_upload_mb == 0) {
                notice.push_str(" Updating the server raises this to 1 GB.");
            }
            self.notice = Some(notice);
            return Task::none();
        }
        if size == 0 {
            self.notice = Some(format!("{filename} is empty."));
            return Task::none();
        }
        let id = self.next_upload_id;
        self.next_upload_id += 1;
        let sent = Arc::new(AtomicU64::new(0));
        self.pending.push(PendingUpload {
            id,
            filename: filename.clone(),
            state: UploadState::Uploading,
            sent: sent.clone(),
            size,
        });
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        Task::perform(media::upload(http, base, token, channel, filename, source, sent), move |r| {
            Message::Uploaded(id, r)
        })
    }

    fn attach_paths(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let mut tasks = Vec::new();
        for path in paths {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
            if path.is_dir() {
                self.notice = Some(format!("{name} is a folder. Zip it first to send it."));
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) => tasks.push(self.start_upload(name, Source::File(path), m.len())),
                Err(e) => self.notice = Some(format!("Couldn't read {name}: {e}")),
            }
        }
        Task::batch(tasks)
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
                return self.attach_paths(paths);
            }
            Message::PasteShortcut => {
                if !self.can_attach_here() {
                    return Task::none();
                }
                return Task::perform(
                    async { tokio::task::spawn_blocking(read_clipboard).await.ok().flatten() },
                    Message::Pasted,
                );
            }
            Message::Pasted(Some(Clip::Image(png))) => {
                let name = format!("screenshot-{}.png", chrono::Local::now().format("%Y-%m-%d-%H%M%S"));
                let size = png.len() as u64;
                return self.start_upload(name, Source::Bytes(png), size);
            }
            Message::Pasted(Some(Clip::Files(paths))) => {
                // Some file managers also offer the files' names as text, which the message box
                // has already pasted; take that back out.
                let mut text = self.composer.clone();
                for path in &paths {
                    let full = path.display().to_string();
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    for needle in [format!("file://{full}"), full, name] {
                        if !needle.is_empty() && text.contains(&needle) {
                            text = text.replacen(&needle, "", 1);
                            break;
                        }
                    }
                }
                if text != self.composer {
                    self.set_composer(text.trim_end().to_string());
                }
                return self.attach_paths(paths);
            }
            Message::Pasted(None) => {} // plain text paste: the text box handles it
            Message::Uploaded(id, result) => {
                if self.pending_channel != self.session.as_ref().and_then(|s| s.current) {
                    return Task::none();
                }
                let mut preview = None;
                if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
                    p.state = match result {
                        Ok(a) => {
                            if a.is_image() {
                                preview = Some(a.id.clone());
                            }
                            UploadState::Ready(a)
                        }
                        Err(e) => UploadState::Failed(e),
                    };
                }
                // Show a small preview of pictures waiting to be sent.
                if let Some(key) = preview
                    && self.images.start(&key)
                    && let Some((http, base, token)) = self.http_ctx()
                {
                    let k = key.clone();
                    return Task::perform(media::load(http, base, token, key, true), move |r| {
                        Message::PictureLoaded(k.clone(), r)
                    });
                }
            }
            Message::RemovePending(id) => self.pending.retain(|p| p.id != id),
            Message::PictureSeen(key, seen) => {
                let changed = if seen { self.on_screen.insert(key.clone()) } else { self.on_screen.remove(&key) };
                if changed {
                    self.images.set_visible(&self.on_screen);
                }
                // Scrolled to: load it (again, if it was dropped to make room).
                if seen {
                    return self.load_preview(key);
                }
            }
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
                return Task::perform(
                    async move {
                        let file = rfd::AsyncFileDialog::new().set_file_name(&a.filename).save_file().await;
                        (a, file.map(|f| f.path().to_path_buf()))
                    },
                    |(a, path)| Message::SaveTo(a, path),
                );
            }
            Message::SaveTo(_, None) => {}
            Message::SaveTo(a, Some(path)) => {
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                let id = self.next_download_id;
                self.next_download_id += 1;
                let done = Arc::new(AtomicU64::new(0));
                self.downloads.push(Download { id, filename: a.filename.clone(), done: done.clone(), size: a.size });
                return Task::perform(media::save(http, base, token, a.id, path, done), move |r| Message::Saved(id, r));
            }
            Message::Saved(id, result) => {
                self.downloads.retain(|d| d.id != id);
                self.notice = Some(match result {
                    Ok(path) => format!("Saved to {path}"),
                    Err(e) => e,
                });
            }
            Message::CopyShortcut => {
                // Ctrl+C in the picture viewer copies the picture; elsewhere text boxes handle it.
                if let Some(Overlay::Image(a)) = &self.overlay {
                    return self.update_media(Message::CopyImage(a.clone()));
                }
            }
            Message::CopyImage(a) => {
                let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
                self.copying = Some((a.id.clone(), false));
                let id = a.id.clone();
                return Task::perform(
                    async move {
                        let bytes = media::download(http, base, token, a.id, false).await?;
                        let (w, h, rgba) = tokio::task::spawn_blocking(move || media::decode_for_clipboard(&bytes))
                            .await
                            .map_err(|e| e.to_string())??;
                        Ok((w, h, Arc::new(rgba)))
                    },
                    move |r| Message::CopyReady(id.clone(), r),
                );
            }
            Message::CopyReady(id, Ok((width, height, rgba))) => {
                if self.clipboard.is_none() {
                    self.clipboard = arboard::Clipboard::new().ok();
                }
                let bytes = Arc::try_unwrap(rgba).unwrap_or_else(|a| (*a).clone());
                let image = arboard::ImageData { width, height, bytes: bytes.into() };
                match self.clipboard.as_mut().map(|c| c.set_image(image)) {
                    Some(Ok(())) => {
                        // The viewer's button says "Copied"; without the viewer, say so in the notice bar.
                        if self.copying.as_ref().is_some_and(|(c, _)| *c == id) {
                            self.copying = Some((id, true));
                        } else {
                            self.notice = Some("Picture copied. Paste it anywhere with Ctrl+V.".into());
                        }
                    }
                    Some(Err(e)) => self.notice = Some(format!("Couldn't copy the picture: {e}")),
                    None => self.notice = Some("Couldn't open the clipboard.".into()),
                }
            }
            Message::CopyReady(_, Err(e)) => {
                self.copying = None;
                self.notice = Some(format!("Couldn't copy the picture: {e}"));
            }
            Message::TransferTick => {} // just redraws the progress bars
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
                let sent = Arc::new(AtomicU64::new(0));
                self.pending.push(PendingUpload {
                    id,
                    filename: a.filename.clone(),
                    state: UploadState::Uploading,
                    sent: sent.clone(),
                    size: a.size,
                });
                return Task::perform(
                    async move {
                        let bytes = media::download(http.clone(), base.clone(), token.clone(), a.id, false).await?;
                        media::upload(http, base, token, channel, a.filename, Source::Bytes(bytes), sent).await
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

/// What's on the clipboard that can be sent: copied files first, then a picture.
/// What the clipboard holds, for the Paste menu item (Ctrl+V's text is pasted by the text box itself).
#[derive(Debug, Clone)]
pub enum AnyClip {
    Text(String),
    Other(Clip),
}

pub(crate) fn read_clipboard_any() -> Option<AnyClip> {
    if let Some(clip) = read_clipboard() {
        return Some(AnyClip::Other(clip));
    }
    let text = arboard::Clipboard::new().ok()?.get_text().ok()?;
    (!text.is_empty()).then_some(AnyClip::Text(text))
}

fn read_clipboard() -> Option<Clip> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Some(Clip::Files(files));
    }
    let img = clipboard.get_image().ok()?;
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    let mut png = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(Clip::Image(png.into_inner()))
}
