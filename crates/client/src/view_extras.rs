//! Views for pictures, files, emoji, reactions and direct messages.

use iced::widget::{
    Id, button, center, column, container, image, mouse_area, progress_bar, row, scrollable, space, stack, text,
    text_input, tooltip,
};
use iced::{Alignment, Border, Color, ContentFit, Element, Length, Padding};
use std::sync::atomic::Ordering;
use syncord_proto::{Attachment, Message as ChatMessage, User};

use crate::app::{App, Message, PickerTab, PickerTarget, Session, UploadState};
use crate::emoji;
use crate::icons::{self, icon};
use crate::media::{self, format_size};
use crate::style::{self, bg, bold, card, label, semibold};
use crate::theme::{contrast_text, with_alpha};

const PICTURE_W: f32 = 400.0;
const PICTURE_H: f32 = 300.0;
/// GIF tiles in the picker: three to a row.
const GIF_TILE_W: f32 = 120.0;
const GIF_TILE_H: f32 = 96.0;

/// A picture shrunk to fit a fixed-size box, keeping its shape, centred on a backdrop.
fn boxed_picture<'a>(
    p: &media::Picture,
    ms: u64,
    w: f32,
    h: f32,
    radius: f32,
    backdrop: Color,
) -> Element<'a, Message> {
    let (width, height) = p.size();
    let (fw, fh) = media::fit(width, height, w, h);
    center(image(p.frame(ms).clone()).width(fw).height(fh).border_radius(radius))
        .width(w)
        .height(h)
        .style(card(backdrop, radius))
        .into()
}

/// Shorten a long file name in the middle, keeping its end and extension:
/// "2C5273C0-4F53-...7B4235.mp4".
pub fn middle_ellipsis(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    let tail = (max / 3).max(6).min(chars.len());
    let head = max.saturating_sub(tail + 1);
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
}

fn percent(done: u64, total: u64) -> u64 {
    (done * 100).checked_div(total).unwrap_or(0).min(100)
}

impl App {
    // ------------------------------------------------------------------ attachments

    pub(crate) fn attachments_view<'a>(&'a self, m: &'a ChatMessage) -> Option<Element<'a, Message>> {
        if m.attachments.is_empty() {
            return None;
        }
        let mut col = column![].spacing(6);
        for a in &m.attachments {
            col = col.push(if a.is_video() {
                self.video_view(a)
            } else if a.is_image() {
                self.picture_view(m, a)
            } else {
                self.file_card(a)
            });
        }
        Some(col.into())
    }

    fn picture_view<'a>(&'a self, m: &'a ChatMessage, a: &'a Attachment) -> Element<'a, Message> {
        let t = &self.theme;
        let (w, h) = media::fit(a.width.unwrap_or(1), a.height.unwrap_or(1), PICTURE_W, PICTURE_H);
        let radius = t.radius;

        let content: Element<'a, Message> = match self.preview(a) {
            Some(p) => image(p.frame(self.animation_ms(Some(m.id))).clone())
                .width(w)
                .height(h)
                .content_fit(ContentFit::Cover)
                .border_radius(radius)
                .into(),
            None => {
                let msg = if self.picture_failed(a) { "Couldn't load picture" } else { "" };
                center(text(msg).size(self.fs() * 0.8).color(t.muted.0))
                    .width(w)
                    .height(h)
                    .style(card(t.floating.0, radius))
                    .into()
            }
        };

        let mut layers = stack![
            button(content).padding(0).on_press(Message::OpenViewer(a.clone())).style(|_, _| button::Style::default())
        ];
        if a.animated {
            let badge = container(text("GIF").size(11).font(bold()).color(Color::WHITE))
                .padding([1, 5])
                .style(style::flat(Color::from_rgba(0.0, 0.0, 0.0, 0.6), 4.0));
            let mut corner = row![badge].spacing(4).align_y(Alignment::Center);
            if self.hovered == Some(m.id) {
                let fav = self.settings.favorites_here().any(|f| f.attachment.id == a.id);
                let star = if fav { &icons::STAR } else { &icons::STAR_OUTLINE };
                corner = corner.push(
                    button(icon(star, 16.0, if fav { iced::Color::from_rgb8(0xF0, 0xB2, 0x32) } else { Color::WHITE }))
                        .padding(4)
                        .on_press(Message::ToggleFavoriteGif(a.clone()))
                        .style(|_, _| button::Style {
                            background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
                            border: Border { radius: 6.0.into(), ..Default::default() },
                            ..Default::default()
                        }),
                );
            }
            layers = layers.push(container(corner).padding(6).width(w).align_x(Alignment::End));
        }
        layers.into()
    }

    fn file_card<'a>(&'a self, a: &'a Attachment) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let content = row![
            icon(&icons::FILE, 30.0, t.accent.0),
            column![
                text(middle_ellipsis(&a.filename, 26))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .size(fs * 0.95)
                    .color(t.accent.0),
                text(format_size(a.size)).size(fs * 0.75).color(t.muted.0),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(icon(&icons::DOWNLOAD, 20.0, t.text.0))
                .padding(6)
                .on_press(Message::SaveAttachment(a.clone()))
                .style(style::icon_button(t)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        container(content)
            .padding([10, 12])
            .width(360)
            .style(move |_| container::Style {
                background: Some(t.floating.0.into()),
                border: Border { radius: t.radius.into(), width: 1.0, color: t.hover.0 },
                ..Default::default()
            })
            .into()
    }

    // ------------------------------------------------------------------ reactions

    pub(crate) fn reactions_view<'a>(&'a self, s: &'a Session, m: &'a ChatMessage) -> Option<Element<'a, Message>> {
        if m.reactions.is_empty() {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let mut r = row![].spacing(4);
        for reaction in &m.reactions {
            let mine = reaction.users.contains(&s.me);
            let (accent, floating) = (t.accent.0, t.floating.0);
            let pill = button(
                row![
                    style::emoji_text(reaction.emoji.clone(), fs * 1.05),
                    text(reaction.users.len().to_string()).size(fs * 0.8).font(semibold())
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            )
            .padding([2, 8])
            .on_press(Message::ToggleReaction(m.id, reaction.emoji.clone()))
            .style(move |_, status| {
                let hovered = matches!(status, button::Status::Hovered);
                button::Style {
                    background: Some(if mine { with_alpha(accent, 0.25) } else { floating }.into()),
                    text_color: if mine {
                        contrast_text(floating).scale_alpha(0.95)
                    } else {
                        contrast_text(floating).scale_alpha(0.8)
                    },
                    border: Border {
                        radius: 8.0.into(),
                        width: 1.0,
                        color: if mine || hovered { accent } else { Color::TRANSPARENT },
                    },
                    ..Default::default()
                }
            });
            let names: Vec<&str> = reaction.users.iter().filter_map(|u| s.users.get(u).map(User::name)).collect();
            let who = container(text(format!("{} reacted with {}", names.join(", "), reaction.emoji)).size(fs * 0.8))
                .padding([4, 8])
                .style(card(t.rail.0, 6.0));
            r = r.push(tooltip(pill, who, tooltip::Position::Top));
        }
        Some(r.wrap().vertical_spacing(4).into())
    }

    // ------------------------------------------------------------------ composer extras

    /// A thin progress bar in the theme's colours.
    fn progress<'a>(&self, done: u64, total: u64, width: f32) -> Element<'a, Message> {
        let (accent, track) = (self.theme.accent.0, self.theme.rail.0);
        let value = if total == 0 { 0.0 } else { (done as f64 / total as f64).clamp(0.0, 1.0) as f32 };
        progress_bar(0.0..=1.0, value)
            .length(width)
            .girth(4)
            .style(move |_| progress_bar::Style {
                background: track.into(),
                bar: accent.into(),
                border: Border { radius: 2.0.into(), ..Default::default() },
            })
            .into()
    }

    /// Files waiting to be sent with the next message, and files being saved.
    pub(crate) fn pending_view(&self) -> Option<Element<'_, Message>> {
        if self.pending.is_empty() && self.downloads.is_empty() {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let mut r = row![].spacing(8);
        for p in &self.pending {
            let sent = p.sent.load(Ordering::Relaxed).min(p.size);
            let mut info = column![text(crate::view::truncate(&p.filename, 28)).size(fs * 0.85).color(t.text.0)]
                .spacing(3)
                .width(180);
            info = match &p.state {
                UploadState::Uploading => info.push(self.progress(sent, p.size, 180.0)).push(
                    text(format!("{}% · {} of {}", percent(sent, p.size), format_size(sent), format_size(p.size)))
                        .size(fs * 0.72)
                        .color(t.muted.0),
                ),
                UploadState::Ready(a) => info.push(text(format_size(a.size)).size(fs * 0.72).color(t.online.0)),
                UploadState::Failed(e) => info.push(text(e.clone()).size(fs * 0.72).color(t.danger.0)),
            };
            let thumb: Element<'_, Message> = match &p.state {
                UploadState::Ready(a) if self.preview(a).is_some() => {
                    boxed_picture(self.preview(a).unwrap(), 0, 40.0, 40.0, 4.0, t.rail.0)
                }
                _ => icon(&icons::FILE, 22.0, t.muted.0),
            };
            let chip = row![
                thumb,
                info,
                button(icon(&icons::CLOSE, 14.0, t.muted.0))
                    .padding(4)
                    .on_press(Message::RemovePending(p.id))
                    .style(style::icon_button(t)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            r = r.push(container(chip).padding([6, 8]).style(card(t.floating.0, t.radius)));
        }
        for d in &self.downloads {
            let done = d.done.load(Ordering::Relaxed).min(d.size);
            let chip = row![
                icon(&icons::DOWNLOAD, 20.0, t.muted.0),
                column![
                    text(format!("Saving {}", crate::view::truncate(&d.filename, 24))).size(fs * 0.85).color(t.text.0),
                    self.progress(done, d.size, 180.0),
                    text(format!("{}% · {} of {}", percent(done, d.size), format_size(done), format_size(d.size)))
                        .size(fs * 0.72)
                        .color(t.muted.0),
                ]
                .spacing(3)
                .width(180),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            r = r.push(container(chip).padding([6, 8]).style(card(t.floating.0, t.radius)));
        }
        Some(r.wrap().vertical_spacing(8).into())
    }

    /// Suggestions while typing `:emoji` or `@someone`.
    pub(crate) fn suggestions_view(&self) -> Option<Element<'_, Message>> {
        use crate::composer::Suggestion;
        let found = self.composer_suggestions();
        let s = self.session.as_ref()?;
        if found.is_empty() {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let title = if matches!(found[0], Suggestion::Emoji(..)) { "Emoji matching" } else { "Members" };
        let selected = self.suggestion_index.min(found.len() - 1);
        let mut col = column![label(t, title, fs)].spacing(2);
        for (i, pick) in found.into_iter().enumerate() {
            let line: Element<'_, Message> = match pick {
                Suggestion::Emoji(e, code) => row![
                    style::emoji_text(e.as_str(), fs * 1.2),
                    text(format!(":{code}:")).size(fs * 0.9).color(t.text.0)
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into(),
                Suggestion::User(id) => match s.users.get(&id) {
                    Some(u) => row![
                        self.avatar(u, 24.0),
                        text(u.name().to_owned()).size(fs * 0.95).color(t.header.0),
                        text(u.username.clone()).size(fs * 0.8).color(t.muted.0),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center)
                    .into(),
                    None => continue,
                },
                Suggestion::Role(id) => match s.role(id) {
                    Some(r) => {
                        let color = r.color.map(crate::theme::rgb).unwrap_or(t.text.0);
                        row![
                            text(format!("@{}", r.name)).size(fs * 0.95).font(semibold()).color(color),
                            text("Notify everyone with this role").size(fs * 0.8).color(t.muted.0),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center)
                        .into()
                    }
                    None => continue,
                },
                Suggestion::Everyone => row![
                    text("@everyone").size(fs * 0.95).font(semibold()).color(t.header.0),
                    text("Notify everyone who can see this channel").size(fs * 0.8).color(t.muted.0),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into(),
            };
            col = col.push(
                button(line)
                    .width(Length::Fill)
                    .padding([4, 8])
                    .on_press(Message::AcceptSuggestion(i))
                    .style(style::row_button(t, i == selected)),
            );
        }
        col = col.push(text("Tab or Enter to pick, Up and Down to move").size(fs * 0.7).color(t.muted.0));
        Some(container(col).padding(8).width(Length::Fill).style(card(t.floating.0, t.radius)).into())
    }

    /// The emoji picker (and favourite GIFs), shown above the message box.
    pub(crate) fn emoji_picker_view(&self) -> Option<Element<'_, Message>> {
        let picker = self.emoji_picker.as_ref()?;
        let t = &self.theme;
        let fs = self.fs();

        let tab_button = |content: Element<'static, Message>, tab: PickerTab| {
            button(content)
                .padding([4, 5])
                .on_press(Message::PickerTab(tab))
                .style(style::row_button(t, picker.tab == tab && (picker.search.is_empty() || tab == PickerTab::Gifs)))
        };
        let mut tabs = row![].spacing(2);
        if !self.settings.recent_emoji.is_empty() {
            tabs = tabs.push(tab_button(style::emoji_text("🕘", fs * 1.1).into(), PickerTab::Emoji(usize::MAX)));
        }
        for (i, (_, _, glyph)) in emoji::GROUPS.iter().enumerate() {
            tabs = tabs.push(tab_button(style::emoji_text(*glyph, fs * 1.1).into(), PickerTab::Emoji(i)));
        }
        if picker.target == PickerTarget::Composer {
            tabs = tabs.push(tab_button(text("GIF").size(fs * 0.8).font(bold()).into(), PickerTab::Gifs));
        }

        let gifs_tab = picker.tab == PickerTab::Gifs;
        let provider = self.session.as_ref().and_then(|s| s.gif_provider.as_deref());
        let placeholder = match (gifs_tab, provider) {
            (true, Some(p)) => format!("Search {p}"),
            (true, None) => "Search your favourite GIFs".to_string(),
            (false, _) => "Find the perfect emoji".to_string(),
        };
        let search = text_input(&placeholder, &picker.search)
            .id(Id::new("emoji-search"))
            .on_input(Message::EmojiSearch)
            .padding(8)
            .size(fs * 0.9)
            .style(style::input(t, t.rail.0));

        let body: Element<'_, Message> = if gifs_tab {
            self.gifs_view(&picker.search, provider)
        } else {
            let (title, list): (String, Vec<String>) = if !picker.search.is_empty() {
                (
                    "Search results".into(),
                    emoji::search(&picker.search, 120).into_iter().map(|e| e.as_str().to_string()).collect(),
                )
            } else {
                match picker.tab {
                    PickerTab::Emoji(usize::MAX) => ("Recently used".into(), self.settings.recent_emoji.clone()),
                    PickerTab::Emoji(i) => {
                        (emoji::GROUPS[i.min(8)].1.into(), emoji::group(i).map(|e| e.as_str().to_string()).collect())
                    }
                    PickerTab::Gifs => (String::new(), Vec::new()),
                }
            };
            let mut grid = row![].spacing(2);
            for e in list {
                grid = grid.push(
                    button(center(style::emoji_text(e.clone(), fs * 1.5)).width(38).height(38))
                        .padding(0)
                        .on_press(Message::PickEmoji(e))
                        .style(style::row_button(t, false)),
                );
            }
            column![
                label(t, title, fs),
                scrollable(grid.wrap().vertical_spacing(2)).height(Length::Fill).style(style::scroll(t))
            ]
            .spacing(6)
            .into()
        };

        let panel = column![search, tabs.wrap(), body].spacing(8).height(Length::Fill);
        Some(
            container(panel)
                .padding(10)
                .width(420)
                .height(400)
                .style(move |_| container::Style {
                    background: Some(t.floating.0.into()),
                    border: Border { radius: (t.radius * 1.2).into(), width: 1.0, color: t.hover.0 },
                    ..Default::default()
                })
                .into(),
        )
    }

    /// The GIF tab: favourites and GIF search.
    fn gifs_view<'a>(&'a self, search: &str, provider: Option<&'a str>) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let search = search.trim();
        let mut col = column![].spacing(8);

        // Favourites (filtered by name while searching).
        let needle = search.to_lowercase();
        let favorites: Vec<&Attachment> = self
            .settings
            .favorites_here()
            .map(|f| &f.attachment)
            .filter(|a| needle.is_empty() || a.filename.to_lowercase().contains(&needle))
            .collect();
        if !favorites.is_empty() {
            let mut grid = row![].spacing(6);
            for a in favorites {
                let content: Element<'_, Message> = match self.images.picture(&a.id) {
                    Some(p) => boxed_picture(p, self.animation_ms(None), GIF_TILE_W, GIF_TILE_H, 6.0, t.rail.0),
                    None => container(space()).width(GIF_TILE_W).height(GIF_TILE_H).style(card(t.rail.0, 6.0)).into(),
                };
                // A filled star on each favourite: click it to remove it (or right-click the GIF).
                let gold = Color::from_rgb8(0xF0, 0xB2, 0x32);
                let star = button(icon(&icons::STAR, 15.0, gold))
                    .padding(4)
                    .on_press(Message::ToggleFavoriteGif(a.clone()))
                    .style(|_, _| button::Style {
                        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
                        border: Border { radius: 6.0.into(), ..Default::default() },
                        ..Default::default()
                    });
                let tile = stack![
                    button(content)
                        .padding(0)
                        .on_press(Message::SendFavorite(a.clone()))
                        .style(|_, _| button::Style::default()),
                    container(tooltip(
                        star,
                        container(text("Remove from favourites").size(fs * 0.75))
                            .padding([3, 6])
                            .style(card(t.rail.0, 4.0)),
                        tooltip::Position::Top,
                    ))
                    .padding(4)
                    .width(GIF_TILE_W)
                    .align_x(Alignment::End),
                ];
                let fav = a.clone();
                grid = grid.push(crate::widgets::context_menu(tile, move || {
                    container(
                        button(text("Remove from favourites").size(fs * 0.9).color(t.text.0))
                            .padding([6, 10])
                            .on_press(Message::ToggleFavoriteGif(fav.clone()))
                            .style(style::row_button(t, false)),
                    )
                    .padding(6)
                    .style(card(style::darken(t.floating.0, 0.12), t.radius * 0.8))
                    .into()
                }));
            }
            col = col.push(label(t, "Favourites", fs)).push(grid.wrap().vertical_spacing(6));
        }

        match provider {
            None => {
                col = col.push(
                    container(
                        text(
                            "GIF search isn't set up on this server yet. The server owner can add a free GIPHY or \
                             KLIPY key to syncord-server.toml. Meanwhile, star GIFs in the chat to keep them here.",
                        )
                        .size(fs * 0.85)
                        .color(t.muted.0),
                    )
                    .padding([8, 0]),
                );
            }
            Some(provider) => {
                let title = if search.is_empty() { format!("Trending on {provider}") } else { "Results".to_string() };
                col = col.push(label(t, title, fs));
                let shown = self.gifs.query.as_deref() == Some(search);
                if let Some(e) = &self.gifs.error {
                    col = col.push(text(e.clone()).size(fs * 0.85).color(t.danger.0));
                } else if !shown || self.gifs.waiting.is_some() && self.gifs.results.is_empty() {
                    col = col.push(text("Searching...").size(fs * 0.85).color(t.muted.0));
                } else if self.gifs.results.is_empty() {
                    col = col.push(text("No GIFs found. Try other words.").size(fs * 0.85).color(t.muted.0));
                }
                if shown || !self.gifs.results.is_empty() {
                    let mut grid = row![].spacing(6);
                    for r in &self.gifs.results {
                        let hovered = self.gifs.hovered.as_deref() == Some(r.id.as_str());
                        let picture = hovered
                            .then(|| self.images.picture(&media::gif_key(&r.id, true)))
                            .flatten()
                            .or_else(|| self.images.picture(&media::gif_key(&r.id, false)));
                        let content: Element<'_, Message> = match picture {
                            Some(p) => boxed_picture(p, self.animation_ms(None), GIF_TILE_W, GIF_TILE_H, 6.0, t.rail.0),
                            None => container(space())
                                .width(GIF_TILE_W)
                                .height(GIF_TILE_H)
                                .style(card(t.rail.0, 6.0))
                                .into(),
                        };
                        let tile = button(content).padding(0).on_press(Message::SendGif(r.id.clone())).style(
                            move |_, status| button::Style {
                                border: Border {
                                    radius: 6.0.into(),
                                    width: if matches!(status, button::Status::Hovered) { 2.0 } else { 0.0 },
                                    color: t.accent.0,
                                },
                                ..Default::default()
                            },
                        );
                        grid = grid.push(
                            mouse_area(tooltip(
                                tile,
                                container(text(r.title.clone()).size(fs * 0.75))
                                    .padding([3, 6])
                                    .style(card(t.rail.0, 4.0)),
                                tooltip::Position::Bottom,
                            ))
                            .on_enter(Message::GifHover(r.id.clone(), true))
                            .on_exit(Message::GifHover(r.id.clone(), false)),
                        );
                    }
                    col = col.push(grid.wrap().vertical_spacing(6));
                }
            }
        }

        let mut page = column![scrollable(col).spacing(4).height(Length::Fill).style(style::scroll(t))].spacing(4);
        if let Some(provider) = provider {
            page = page.push(
                container(text(format!("Powered by {provider}")).size(fs * 0.7).font(bold()).color(t.muted.0))
                    .width(Length::Fill)
                    .align_x(Alignment::End),
            );
        }
        page.into()
    }

    // ------------------------------------------------------------------ overlays

    pub(crate) fn image_viewer<'a>(&'a self, a: &'a Attachment) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let full = self.images.picture(&media::key(&a.id, true)).or_else(|| self.preview(a));
        let picture: Element<'a, Message> = match full {
            Some(p) => image(p.frame(self.animation_ms(None)).clone())
                .content_fit(ContentFit::ScaleDown)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            None => center(text("Loading...").color(Color::WHITE)).into(),
        };
        let bar = row![
            text(a.filename.clone()).size(fs * 0.9).color(Color::WHITE).width(Length::Fill),
            text(format!("{} × {} · {}", a.width.unwrap_or(0), a.height.unwrap_or(0), format_size(a.size)))
                .size(fs * 0.8)
                .color(with_alpha(Color::WHITE, 0.7)),
            button(
                row![icon(&icons::DOWNLOAD, 16.0, t.header.0), text("Save").size(fs * 0.85)]
                    .spacing(6)
                    .align_y(Alignment::Center)
            )
            .padding([6, 12])
            .on_press(Message::SaveAttachment(a.clone()))
            .style(style::solid_button(t.floating.0, t.radius * 0.6)),
            tooltip(
                button(
                    text(match &self.copying {
                        Some((id, false)) if *id == a.id => "Copying...",
                        Some((id, true)) if *id == a.id => "Copied!",
                        _ => "Copy",
                    })
                    .size(fs * 0.85)
                )
                .padding([6, 12])
                .on_press(Message::CopyImage(a.clone()))
                .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                container(text("Copy the picture (Ctrl+C)").size(fs * 0.75)).padding([3, 6]).style(card(t.rail.0, 4.0)),
                tooltip::Position::Top,
            ),
            button(icon(&icons::CLOSE, 18.0, Color::WHITE))
                .padding(6)
                .on_press(Message::CloseOverlay)
                .style(|_, _| button::Style::default()),
        ]
        .spacing(14)
        .align_y(Alignment::Center);
        // Clicks on the picture itself shouldn't close the viewer.
        column![
            container(iced::widget::opaque(mouse_area(picture))).padding(24).height(Length::Fill),
            container(bar).padding([10, 24])
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    pub(crate) fn new_direct_dialog<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut users: Vec<&User> = s.users.values().filter(|u| u.id != s.me).collect();
        users.sort_by_key(|u| (!s.online.contains(&u.id), u.name().to_lowercase()));
        let mut list = column![].spacing(2);
        for u in users {
            let online = s.online.contains(&u.id);
            list = list.push(
                button(
                    row![
                        self.avatar_status(u, 32.0, online, t.chat.0),
                        column![
                            text(u.name().to_owned()).size(fs).font(semibold()),
                            text(u.username.clone()).size(fs * 0.75).color(t.muted.0)
                        ],
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([6, 8])
                .on_press(Message::OpenDirect(u.id))
                .style(style::row_button(t, false)),
            );
        }
        let body = column![
            text("Start a conversation").size(fs * 1.3).font(bold()).color(t.header.0),
            text("Only the two of you can see direct messages.").size(fs * 0.85).color(t.muted.0),
            scrollable(list).height(320).style(style::scroll(t)),
        ]
        .spacing(10);
        container(body).width(420).padding(20).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    // ------------------------------------------------------------------ direct messages sidebar

    pub(crate) fn home_sidebar<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let header = container(text("Direct Messages").size(fs * 1.05).font(bold()).color(t.header.0))
            .padding([0, 16])
            .height(48)
            .width(Length::Fill)
            .align_y(Alignment::Center);

        let category = row![label(t, "Conversations", fs), space::horizontal()]
            .push(
                button(text("+").size(fs * 1.2))
                    .padding([0, 6])
                    .on_press(Message::NewDirect)
                    .style(style::link_button(t.muted.0, t.header.0)),
            )
            .align_y(Alignment::Center);
        let mut list =
            column![container(category).padding(Padding { left: 8.0, top: 16.0, right: 4.0, bottom: 4.0 })].spacing(2);
        if s.directs.is_empty() {
            list = list.push(
                container(
                    text("No conversations yet. Click + or someone's profile to send them a message.")
                        .size(fs * 0.85)
                        .color(t.muted.0),
                )
                .padding(8),
            );
        }
        for d in &s.directs {
            let Some(u) = s.users.get(&d.user_id) else { continue };
            let selected = s.current == Some(d.id);
            let unread = s.is_unread(d.id) && !selected;
            let mut name = text(u.name().to_owned()).size(fs * 0.95);
            if unread {
                name = name.font(bold()).color(t.header.0);
            }
            let mut line = row![
                self.with_user_menu(self.avatar_status(u, 32.0, s.online.contains(&u.id), t.sidebar.0), u.id),
                name.width(Length::Fill)
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            if s.mention_count(d.id) > 0 && !selected {
                line = line.push(crate::view::mention_badge(t, s.mention_count(d.id), fs));
            } else if unread {
                line = line.push(container(space()).width(8).height(8).style(style::flat(t.header.0, 4.0)));
            }
            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding([5, 8])
                    .on_press(Message::SelectChannel(d.id))
                    .style(style::row_button(t, selected)),
            );
        }

        container(column![
            header,
            style::divider(style::darken(t.sidebar.0, 0.25)),
            scrollable(container(list).padding([0, 8])).height(Length::Fill).style(style::scroll(t)),
            self.voice_panel(s),
            self.user_panel(s),
        ])
        .width(240)
        .height(Length::Fill)
        .style(bg(t.sidebar.0))
        .into()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn long_file_names_keep_their_end() {
        let n = super::middle_ellipsis("2C5273C0-4F53-4A82-9ABE-99A43A7B4235.mp4", 34);
        assert!(n.chars().count() <= 34, "{n}");
        assert!(n.ends_with(".mp4") && n.starts_with("2C5273C0") && n.contains('…'), "{n}");
        assert_eq!(super::middle_ellipsis("cat.png", 34), "cat.png");
    }
}
