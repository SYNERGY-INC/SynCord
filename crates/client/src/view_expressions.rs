//! Drawing the server's emoji and stickers (in messages, reactions and the picker), and
//! the Emoji, Soundboard and Overview pages of the server settings.

use celerity_proto::expressions::{EmojiRef, find_custom_emoji};
use celerity_proto::{CustomEmoji, EmojiId, Sticker, perms};
use iced::widget::{Id, button, center, column, container, rich_text, row, space, text, text_input, tooltip};
use iced::{Alignment, Element, Font, Length};

use crate::app::{App, Message, PickerTarget, Session};
use crate::app_expressions::{ExprKind, ExprMsg};
use crate::icons;
use crate::style::{self, bold, card, label, semibold};
use crate::view_messages::pieces;

fn ex(m: ExprMsg) -> Message {
    Message::Expressions(m)
}

/// Words in a line of text, each keeping the spaces after it (so a wrapping row flows like text).
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split_inclusive(' ')
}

impl App {
    /// A server emoji as a picture (its `:name:` until the picture has loaded).
    pub(crate) fn custom_emoji<'a>(&self, id: EmojiId, name: &str, size: f32) -> Element<'a, Message> {
        let key = self.expression_key(ExprKind::Emoji, id);
        match self.images.picture(&key) {
            Some(p) => crate::widgets::picture(p.clone(), size, size, false, 0.0, true),
            None => text(format!(":{name}:")).size(size * 0.6).color(self.theme.muted.0).into(),
        }
    }

    /// A reaction or picker entry: a standard emoji, or a server emoji's code.
    pub(crate) fn emoji_glyph<'a>(&self, emoji: &str, size: f32) -> Element<'a, Message> {
        match single_code(emoji) {
            Some(r) => self.custom_emoji(r.id, r.name, size),
            None => style::emoji_text(emoji.to_string(), size).into(),
        }
    }

    /// The three emoji you used last that work here, for one-click reactions.
    pub(crate) fn quick_reactions(&self, s: &Session) -> Vec<String> {
        let usable = |e: &&String| match single_code(e) {
            Some(r) => s.emojis.iter().any(|x| x.id == r.id),
            None => emojis::get(e).is_some(),
        };
        self.settings.recent_emoji.iter().filter(usable).take(3).cloned().collect()
    }

    /// Message text with server emoji in it, as pictures in the flow of the words.
    /// `None` when there are none (the usual text drawing is used then).
    pub(crate) fn text_with_custom_emoji<'a>(
        &'a self,
        s: &'a Session,
        content: &'a str,
        size: f32,
    ) -> Option<Element<'a, Message>> {
        let found = find_custom_emoji(content);
        if found.is_empty() {
            return None;
        }
        let t = &self.theme;
        // Just emoji (and spaces): shown big, like other emoji.
        let mut rest = content.to_string();
        for r in found.iter().rev() {
            rest.replace_range(r.range.clone(), "");
        }
        let jumbo = rest.trim().is_empty() && found.len() <= 27;
        let emoji_size = if jumbo { 48.0 } else { size * 1.375 };
        let mut lines = column![].spacing(2);
        for (line_start, line) in line_ranges(content) {
            let mut flow = row![].align_y(Alignment::Center);
            let mut at = line_start;
            let line_end = line_start + line.len();
            let spans_of = |piece: &'a str| -> Element<'a, Message> {
                let spans: Vec<iced::widget::text::Span<'a, Message, Font>> =
                    pieces(piece).into_iter().map(|p| self.piece_span(s, p)).collect();
                rich_text(spans).size(size).color(t.text.0).on_link_click(|m| m).into()
            };
            for r in found.iter().filter(|r| r.range.start >= line_start && r.range.end <= line_end) {
                for w in words(&content[at..r.range.start]) {
                    flow = flow.push(spans_of(w));
                }
                let name = format!(":{}:", r.name);
                flow = flow.push(tooltip(
                    container(self.custom_emoji(r.id, r.name, emoji_size)).padding([0, 1]),
                    container(text(name).size(size * 0.8)).padding([3, 6]).style(card(t.floating.0, 6.0)),
                    tooltip::Position::Top,
                ));
                at = r.range.end;
            }
            for w in words(&content[at..line_end]) {
                flow = flow.push(spans_of(w));
            }
            lines = lines.push(flow.wrap());
        }
        Some(lines.into())
    }

    /// A sticker sent as a message.
    pub(crate) fn sticker_view<'a>(&'a self, st: &'a Sticker) -> Element<'a, Message> {
        let t = &self.theme;
        let key = self.expression_key(ExprKind::Sticker, st.id);
        let picture: Element<'a, Message> = match self.images.picture(&key) {
            Some(p) => crate::widgets::picture(p.clone(), 160.0, 160.0, false, 0.0, true),
            None => center(text(st.name.clone()).size(self.fs() * 0.85).color(t.muted.0))
                .width(160)
                .height(160)
                .style(card(t.floating.0, t.radius))
                .into(),
        };
        tooltip(
            picture,
            container(text(st.name.clone()).size(self.fs() * 0.8)).padding([3, 6]).style(card(t.floating.0, 6.0)),
            tooltip::Position::Right,
        )
        .into()
    }

    /// The picker's page of server emoji.
    pub(crate) fn custom_emoji_grid<'a>(&'a self, list: Vec<&'a CustomEmoji>) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut grid = row![].spacing(2);
        for e in list {
            let tile = button(center(self.custom_emoji(e.id, &e.name, 30.0)).width(38).height(38))
                .padding(0)
                .on_press(Message::PickEmoji(e.code()))
                .style(style::row_button(t, false));
            grid = grid.push(tooltip(
                tile,
                container(text(format!(":{}:", e.name)).size(fs * 0.75)).padding([3, 6]).style(card(t.rail.0, 4.0)),
                tooltip::Position::Top,
            ));
        }
        grid.wrap().vertical_spacing(2).into()
    }

    /// The picker's Stickers page: click one to send it.
    pub(crate) fn stickers_grid<'a>(&'a self, s: &'a Session, search: &str) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let q = search.trim().to_lowercase();
        let mut grid = row![].spacing(6);
        let mut any = false;
        for st in s.stickers.iter().filter(|x| q.is_empty() || x.name.to_lowercase().contains(&q)) {
            any = true;
            let key = self.expression_key(ExprKind::Sticker, st.id);
            let content: Element<'a, Message> = match self.images.picture(&key) {
                Some(p) => crate::widgets::picture(p.clone(), 88.0, 88.0, false, 0.0, true),
                None => container(space()).width(88).height(88).style(card(t.rail.0, 6.0)).into(),
            };
            grid = grid.push(tooltip(
                button(content).padding(4).on_press(ex(ExprMsg::SendSticker(st.id))).style(style::row_button(t, false)),
                container(text(st.name.clone()).size(fs * 0.75)).padding([3, 6]).style(card(t.rail.0, 4.0)),
                tooltip::Position::Top,
            ));
        }
        if !any {
            return text("No stickers match.").size(fs * 0.85).color(t.muted.0).into();
        }
        grid.wrap().vertical_spacing(6).into()
    }

    /// Picker tabs for the server's emoji and stickers.
    pub(crate) fn expression_tab_icons(
        &self,
        target: PickerTarget,
    ) -> (Option<Element<'static, Message>>, Option<Element<'static, Message>>) {
        let Some(s) = &self.session else { return (None, None) };
        let fs = self.fs();
        let emoji = s.emojis.first().map(|e| self.custom_emoji(e.id, &e.name, fs * 1.2));
        let stickers = (target == PickerTarget::Composer && !s.stickers.is_empty())
            .then(|| icons::icon(&icons::STAR, fs * 1.1, self.theme.muted.0));
        (emoji, stickers)
    }

    // ------------------------------------------------------------------ settings pages

    fn upload_row<'a>(&'a self, kind: ExprKind, label_text: &'static str, enabled: bool) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let busy = self.expr.uploading == Some(kind);
        row![
            button(text(if busy { "Uploading..." } else { label_text }).size(fs * 0.9))
                .padding([8, 16])
                .on_press_maybe((enabled && self.expr.uploading.is_none()).then_some(ex(ExprMsg::Upload(kind))))
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        ]
        .into()
    }

    /// Name being edited, or the name with Rename and Delete buttons.
    fn expression_row<'a>(
        &'a self,
        kind: ExprKind,
        id: i64,
        picture: Element<'a, Message>,
        name: String,
        detail: String,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let editing = self.expr.editing.as_ref().filter(|e| e.kind == kind && e.id == id);
        let middle: Element<'a, Message> = match editing {
            Some(e) => {
                let mut r = row![
                    text_input("Name", &e.name)
                        .id(Id::new("expr-name"))
                        .on_input(|v| ex(ExprMsg::EditName(v)))
                        .on_submit(ex(ExprMsg::SaveEdit))
                        .padding(6)
                        .size(fs * 0.9)
                        .style(style::input(t, t.rail.0))
                ]
                .spacing(6)
                .align_y(Alignment::Center);
                if kind == ExprKind::Sound {
                    r = r.push(
                        text_input("Emoji", &e.emoji)
                            .on_input(|v| ex(ExprMsg::EditEmoji(v)))
                            .on_submit(ex(ExprMsg::SaveEdit))
                            .padding(6)
                            .size(fs * 0.9)
                            .width(70)
                            .style(style::input(t, t.rail.0)),
                    );
                }
                r.push(
                    button(text("Save").size(fs * 0.8))
                        .padding([4, 10])
                        .on_press(ex(ExprMsg::SaveEdit))
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                )
                .push(
                    button(text("Cancel").size(fs * 0.8))
                        .padding([4, 8])
                        .on_press(ex(ExprMsg::CancelEdit))
                        .style(style::link_button(t.text.0, t.header.0)),
                )
                .into()
            }
            None => column![
                text(name).size(fs * 0.95).font(semibold()).color(t.header.0),
                text(detail).size(fs * 0.75).color(t.muted.0),
            ]
            .spacing(1)
            .into(),
        };
        let mut line =
            row![container(picture).width(56).align_x(Alignment::Center), container(middle).width(Length::Fill)]
                .spacing(12)
                .align_y(Alignment::Center);
        if editing.is_none() {
            let danger = t.danger.0;
            line = line
                .push(
                    button(text("Rename").size(fs * 0.8))
                        .padding([4, 10])
                        .on_press(ex(ExprMsg::StartEdit(kind, id)))
                        .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                )
                .push(
                    button(text("Delete").size(fs * 0.8))
                        .padding([4, 10])
                        .on_press(ex(ExprMsg::AskDelete(kind, id)))
                        .style(style::link_button(danger, style::darken(danger, 0.2))),
                );
        }
        container(line).padding([6, 10]).width(Length::Fill).style(card(t.floating.0, t.radius * 0.8)).into()
    }

    fn uploader(&self, s: &Session, id: celerity_proto::UserId) -> String {
        s.users.get(&id).map(|u| format!("Added by {}", u.name())).unwrap_or_default()
    }

    pub(crate) fn emoji_settings<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let manage = s.can(perms::MANAGE_EMOJI);
        let mut col = column![
            text("Emoji").size(fs * 1.4).font(bold()).color(t.header.0),
            text(format!(
                "Add up to {} emoji anyone here can use, in messages and reactions. Use PNG, GIF or WebP up to 256 KB; \
                 big pictures are shrunk to 128 px. Names are letters, digits and underscores.",
                celerity_proto::expressions::MAX_EMOJI
            ))
            .size(fs * 0.9)
            .color(t.muted.0),
            self.upload_row(ExprKind::Emoji, "Upload Emoji", manage),
        ]
        .spacing(12);
        if let Some(e) = &self.expr.error {
            col = col.push(text(e.clone()).size(fs * 0.85).color(t.danger.0));
        }
        col = col.push(label(t, format!("Emoji — {}", s.emojis.len()), fs));
        if s.emojis.is_empty() {
            col = col.push(text("No emoji yet.").size(fs * 0.85).color(t.muted.0));
        }
        for e in &s.emojis {
            let detail = format!("{}{}", self.uploader(s, e.created_by), if e.animated { " · animated" } else { "" });
            col = col.push(self.expression_row(
                ExprKind::Emoji,
                e.id,
                self.custom_emoji(e.id, &e.name, 32.0),
                format!(":{}:", e.name),
                detail,
            ));
        }
        col = col.push(style::divider(t.hover.0)).push(text("Stickers").size(fs * 1.2).font(bold()).color(t.header.0));
        col = col.push(
            text(format!(
                "Big pictures sent on their own, from the sticker tab of the emoji picker. Up to {} stickers; \
                 PNG, GIF or WebP up to 512 KB.",
                celerity_proto::expressions::MAX_STICKERS
            ))
            .size(fs * 0.9)
            .color(t.muted.0),
        );
        col = col.push(self.upload_row(ExprKind::Sticker, "Upload Sticker", manage));
        if s.stickers.is_empty() {
            col = col.push(text("No stickers yet.").size(fs * 0.85).color(t.muted.0));
        }
        for st in &s.stickers {
            let key = self.expression_key(ExprKind::Sticker, st.id);
            let pic: Element<'a, Message> = match self.images.picture(&key) {
                Some(p) => crate::widgets::picture(p.clone(), 48.0, 48.0, false, 4.0, true),
                None => container(space()).width(48).height(48).style(card(t.rail.0, 4.0)).into(),
            };
            col = col.push(self.expression_row(
                ExprKind::Sticker,
                st.id,
                pic,
                st.name.clone(),
                self.uploader(s, st.created_by),
            ));
        }
        col.into()
    }

    pub(crate) fn soundboard_settings<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let manage = s.can(perms::MANAGE_SOUNDBOARD);
        let mut col = column![
            text("Soundboard").size(fs * 1.4).font(bold()).color(t.header.0),
            text(format!(
                "Short sounds people can play for everyone in their voice channel, from the soundboard button \
                 in the call panel. Up to {} sounds of at most 5 seconds and 1 MB: Ogg (Opus or Vorbis), WAV or MP3.",
                celerity_proto::expressions::MAX_SOUNDS
            ))
            .size(fs * 0.9)
            .color(t.muted.0),
            self.upload_row(ExprKind::Sound, "Upload Sound", manage),
        ]
        .spacing(12);
        if let Some(e) = &self.expr.error {
            col = col.push(text(e.clone()).size(fs * 0.85).color(t.danger.0));
        }
        col = col.push(label(t, format!("Sounds — {}", s.sounds.len()), fs));
        if s.sounds.is_empty() {
            col = col.push(text("No sounds yet.").size(fs * 0.85).color(t.muted.0));
        }
        for x in &s.sounds {
            let glyph = x.emoji.clone().unwrap_or_else(|| "🔊".into());
            let play = button(style::emoji_text(glyph, fs * 1.4))
                .padding([4, 8])
                .on_press(Message::Soundboard(crate::soundboard::SoundMsg::Preview(x.id)))
                .style(style::row_button(t, false));
            let play = tooltip(
                play,
                container(text("Listen").size(fs * 0.75)).padding([3, 6]).style(card(t.rail.0, 4.0)),
                tooltip::Position::Top,
            );
            let length =
                if x.duration_ms > 0 { format!("{:.1} s · ", x.duration_ms as f32 / 1000.0) } else { String::new() };
            col = col.push(self.expression_row(
                ExprKind::Sound,
                x.id,
                play.into(),
                x.name.clone(),
                format!("{length}{}", self.uploader(s, x.created_by)),
            ));
        }
        col.into()
    }

    /// Server settings > Overview: the admin page and invites.
    pub(crate) fn server_settings<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let version = if s.server_version.is_empty() { "unknown".to_string() } else { s.server_version.clone() };
        let mut col = column![
            text("Overview").size(fs * 1.4).font(bold()).color(t.header.0),
            text(s.community.clone()).size(fs * 1.1).font(semibold()).color(t.header.0),
            text(format!("Celerity Server {version}")).size(fs * 0.85).color(t.muted.0),
        ]
        .spacing(12);
        if s.can(perms::CREATE_INVITES) {
            col = col.push(label(t, "Invites", fs)).push(
                row![
                    button(text("Invite People").size(fs * 0.9))
                        .padding([8, 16])
                        .on_press(Message::Invites(crate::app_invites::InviteMsg::Open))
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                    text("Make an invite link, or see and revoke the ones that still work.")
                        .size(fs * 0.85)
                        .color(t.muted.0),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        col = col.push(label(t, "Admin page", fs)).push(
            text(
                "Who's online, how much space the server uses, invite links, members (with kick and ban, and the \
                 banned list) and backups, in your browser. Sign in with your Celerity username and password.",
            )
            .size(fs * 0.85)
            .color(t.muted.0),
        );
        col = col.push(
            button(
                row![
                    icons::icon(&icons::LINK, fs, crate::theme::contrast_text(t.accent.0)),
                    text("Open the Admin Page").size(fs * 0.9)
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([8, 16])
            .on_press(Message::OpenLink(format!("{}/admin", self.server_base())))
            .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        );
        if self.server_base().starts_with("http://") {
            col = col.push(
                text(
                    "This connection isn't encrypted, so the page sends your password in the clear. Prefer it on \
                     the server PC or your home network.",
                )
                .size(fs * 0.8)
                .color(crate::theme::rgb(0xF0B232)),
            );
        }
        col.into()
    }
}

/// A server emoji's code and nothing else.
fn single_code(s: &str) -> Option<EmojiRef<'_>> {
    let mut found = find_custom_emoji(s);
    (found.len() == 1 && found[0].range == (0..s.len())).then(|| found.remove(0))
}

/// Each line of `text` with where it starts.
fn line_ranges(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        out.push((start, line));
        start += line.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_codes() {
        assert_eq!(single_code("<:wave:12>").map(|r| r.id), Some(12));
        assert!(single_code("👍").is_none());
        assert!(single_code("<:wave:12> hi").is_none());
        assert_eq!(line_ranges("a\nbc\n"), vec![(0, "a"), (2, "bc"), (5, "")]);
        assert_eq!(words("hi there  you").collect::<Vec<_>>(), ["hi ", "there ", " ", "you"]);
    }
}
