//! Views for pictures, files, emoji, reactions and direct messages.

use iced::widget::{
    Id, button, center, column, container, image, mouse_area, row, scrollable, space, stack, text, text_input, tooltip,
};
use iced::{Alignment, Border, Color, ContentFit, Element, Length, Padding};
use syncord_proto::{Attachment, Message as ChatMessage, User};

use crate::app::{App, Message, PickerTab, PickerTarget, Session, UploadState};
use crate::emoji;
use crate::icons::{self, icon};
use crate::media::{self, format_size};
use crate::style::{self, BOLD, SEMIBOLD, avatar_with_status, bg, card, label};
use crate::theme::{contrast_text, with_alpha};

const PICTURE_W: f32 = 400.0;
const PICTURE_H: f32 = 300.0;

impl App {
    // ------------------------------------------------------------------ attachments

    pub(crate) fn attachments_view<'a>(&'a self, m: &'a ChatMessage) -> Option<Element<'a, Message>> {
        if m.attachments.is_empty() {
            return None;
        }
        let mut col = column![].spacing(6);
        for a in &m.attachments {
            col = col.push(if a.is_image() { self.picture_view(m, a) } else { self.file_card(a) });
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
            let badge = container(text("GIF").size(11).font(BOLD).color(Color::WHITE))
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
                text(a.filename.clone()).size(fs * 0.95).color(t.accent.0),
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
                    text(reaction.users.len().to_string()).size(fs * 0.8).font(SEMIBOLD)
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

    /// Files waiting to be sent with the next message.
    pub(crate) fn pending_view(&self) -> Option<Element<'_, Message>> {
        if self.pending.is_empty() {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let mut r = row![].spacing(8);
        for p in &self.pending {
            let (status, color) = match &p.state {
                UploadState::Uploading => ("Uploading...".to_string(), t.muted.0),
                UploadState::Ready(a) => (format_size(a.size), t.online.0),
                UploadState::Failed(e) => (e.clone(), t.danger.0),
            };
            let chip = row![
                icon(&icons::FILE, 22.0, t.muted.0),
                column![
                    text(crate::view::truncate(&p.filename, 28)).size(fs * 0.85).color(t.text.0),
                    text(status).size(fs * 0.72).color(color),
                ]
                .spacing(1),
                button(icon(&icons::CLOSE, 14.0, t.muted.0))
                    .padding(4)
                    .on_press(Message::RemovePending(p.id))
                    .style(style::icon_button(t)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            r = r.push(container(chip).padding([6, 8]).style(card(t.floating.0, t.radius)));
        }
        Some(r.wrap().vertical_spacing(8).into())
    }

    /// Emoji suggestions while typing `:code`.
    pub(crate) fn suggestions_view(&self) -> Option<Element<'_, Message>> {
        let found = self.emoji_suggestions();
        if found.is_empty() {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let mut col = column![label(t, "Emoji matching", fs)].spacing(2);
        for (i, (e, code)) in found.into_iter().enumerate() {
            let line =
                row![style::emoji_text(e.as_str(), fs * 1.2), text(format!(":{code}:")).size(fs * 0.9).color(t.text.0)]
                    .spacing(10)
                    .align_y(Alignment::Center);
            col = col.push(
                button(line)
                    .width(Length::Fill)
                    .padding([4, 8])
                    .on_press(Message::PickEmojiSuggestion(e.as_str().to_string()))
                    .style(style::row_button(t, i == 0)),
            );
        }
        col = col.push(text("Enter to pick the highlighted one").size(fs * 0.7).color(t.muted.0));
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
                .style(style::row_button(t, picker.tab == tab && picker.search.is_empty()))
        };
        let mut tabs = row![].spacing(2);
        if !self.settings.recent_emoji.is_empty() {
            tabs = tabs.push(tab_button(style::emoji_text("🕘", fs * 1.1).into(), PickerTab::Emoji(usize::MAX)));
        }
        for (i, (_, _, glyph)) in emoji::GROUPS.iter().enumerate() {
            tabs = tabs.push(tab_button(style::emoji_text(*glyph, fs * 1.1).into(), PickerTab::Emoji(i)));
        }
        if picker.target == PickerTarget::Composer {
            tabs = tabs.push(tab_button(text("GIF").size(fs * 0.8).font(BOLD).into(), PickerTab::Gifs));
        }

        let search = text_input("Find the perfect emoji", &picker.search)
            .id(Id::new("emoji-search"))
            .on_input(Message::EmojiSearch)
            .padding(8)
            .size(fs * 0.9)
            .style(style::input(t, t.rail.0));

        let body: Element<'_, Message> = if picker.tab == PickerTab::Gifs && picker.search.is_empty() {
            self.favorite_gifs_view()
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

    fn favorite_gifs_view(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let favorites: Vec<&Attachment> = self.settings.favorites_here().map(|f| &f.attachment).collect();
        if favorites.is_empty() {
            return center(
                text("Hover a GIF in the chat and click its star to save it here.")
                    .size(fs * 0.9)
                    .color(t.muted.0)
                    .center(),
            )
            .into();
        }
        let mut grid = row![].spacing(6);
        for a in favorites {
            let (w, h) = media::fit(a.width.unwrap_or(1), a.height.unwrap_or(1), 170.0, 120.0);
            let content: Element<'_, Message> = match self.images.picture(&a.id) {
                Some(p) => image(p.frame(self.animation_ms(None)).clone())
                    .width(w)
                    .height(h)
                    .content_fit(ContentFit::Cover)
                    .into(),
                None => container(space()).width(w).height(h).style(card(t.rail.0, 6.0)).into(),
            };
            grid = grid.push(
                button(content)
                    .padding(0)
                    .on_press(Message::SendFavorite(a.clone()))
                    .style(|_, _| button::Style::default()),
            );
        }
        column![
            label(t, "Favourite GIFs (click to send)", fs),
            scrollable(grid.wrap().vertical_spacing(6)).height(Length::Fill).style(style::scroll(t)),
        ]
        .spacing(6)
        .into()
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
                        avatar_with_status(t, u.name(), &u.profile, 32.0, online, t.chat.0),
                        column![
                            text(u.name().to_owned()).size(fs).font(SEMIBOLD),
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
            text("Start a conversation").size(fs * 1.3).font(BOLD).color(t.header.0),
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
        let header = container(text("Direct Messages").size(fs * 1.05).font(BOLD).color(t.header.0))
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
                name = name.font(BOLD).color(t.header.0);
            }
            let mut line = row![
                avatar_with_status(t, u.name(), &u.profile, 32.0, s.online.contains(&u.id), t.sidebar.0),
                name.width(Length::Fill)
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            if unread {
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
