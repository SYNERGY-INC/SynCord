//! Rich link previews (0.7): YouTube videos, posts on X (Twitter) and Spotify songs get
//! cards that look like the real thing.

use celerity_proto::Embed;
use iced::widget::{button, column, container, row, space, stack, text};
use iced::{Alignment, Border, Color, Element, Length, Padding};

use crate::app::{App, Message};
use crate::icons;
use crate::media;
use crate::style::{self, bold, semibold};
use crate::theme::rgb;

const YOUTUBE_RED: u32 = 0xFF0033;
const SPOTIFY_GREEN: u32 = 0x1DB954;

/// "3:33", or "1:02:03" for long videos.
pub fn clock(secs: u32) -> String {
    crate::app::format_duration(std::time::Duration::from_secs(secs as u64))
}

/// "Name (@handle)" as the name and the handle.
pub fn split_author(author: &str) -> (&str, Option<&str>) {
    match author.rsplit_once(" (") {
        Some((name, rest)) if rest.ends_with(')') => (name.trim(), Some(&rest[..rest.len() - 1])),
        _ => (author.trim(), None),
    }
}

impl App {
    /// A special card for links the server knows more about, if this is one.
    pub(crate) fn rich_embed<'a>(&'a self, e: &'a Embed) -> Option<Element<'a, Message>> {
        match e.provider.as_deref()? {
            "youtube" => Some(self.youtube_card(e)),
            "twitter" => Some(self.post_card(e)),
            "spotify" => Some(self.spotify_card(e)),
            _ => None,
        }
    }

    /// A card with a coloured left edge, like the plain link previews.
    fn edged<'a>(&self, body: Element<'a, Message>, edge: Color, width: f32) -> Element<'a, Message> {
        let floating = style::darken(self.theme.chat.0, 0.18);
        let inner = container(body).padding([10, 12]).width(Length::Fill).style(move |_| container::Style {
            background: Some(floating.into()),
            border: Border { radius: iced::border::right(6.0), ..Default::default() },
            ..Default::default()
        });
        container(inner)
            .padding(Padding { left: 4.0, ..Padding::ZERO })
            .width(width)
            .style(move |_| container::Style {
                background: Some(edge.into()),
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into()
    }

    fn link_text<'a>(&self, label: String, url: &str, size: f32, color: Color) -> Element<'a, Message> {
        button(text(label).size(size).font(semibold()).color(color))
            .padding(0)
            .on_press(Message::OpenLink(url.to_string()))
            .style(style::link_button(color, color))
            .into()
    }

    /// The preview picture, or a dark box while it loads.
    fn embed_thumb<'a>(&'a self, e: &'a Embed, w: f32, h: f32, cover: bool) -> Element<'a, Message> {
        let radius = self.theme.radius * 0.6;
        match e.image.as_ref().and_then(|a| self.preview(a)) {
            Some(p) => crate::widgets::picture(p.clone(), w, h, cover, radius, true),
            None => container(space())
                .width(w)
                .height(h)
                .style(style::flat(Color::from_rgb8(0x10, 0x10, 0x12), radius))
                .into(),
        }
    }

    fn youtube_card<'a>(&'a self, e: &'a Embed) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let link = Color::from_rgb8(0x00, 0xA8, 0xFC);
        let mut body = column![text("YouTube").size(fs * 0.75).color(t.muted.0)].spacing(6);
        if let Some(author) = &e.author {
            body = body.push(match &e.author_url {
                Some(url) => self.link_text(author.clone(), url, fs * 0.8, t.header.0),
                None => text(author.clone()).size(fs * 0.8).font(semibold()).color(t.header.0).into(),
            });
        }
        if let Some(title) = &e.title {
            body = body.push(self.link_text(title.clone(), &e.url, fs * 0.95, link));
        }
        // A big thumbnail with a play button and the length: click to watch.
        let (w, h) = (400.0, 225.0);
        let play = container(icons::icon(&icons::PLAY, 26.0, Color::WHITE))
            .center_x(68)
            .center_y(48)
            .style(style::flat(rgb(YOUTUBE_RED), 14.0));
        let mut layers = stack![self.embed_thumb(e, w, h, true), container(play).center_x(w).center_y(h)];
        if let Some(secs) = e.duration_secs.filter(|s| *s > 0) {
            let badge = container(text(clock(secs)).size(fs * 0.75).font(bold()).color(Color::WHITE))
                .padding([2, 6])
                .style(style::flat(Color::from_rgba(0.0, 0.0, 0.0, 0.8), 4.0));
            layers = layers
                .push(container(badge).padding(8).width(w).height(h).align_x(Alignment::End).align_y(Alignment::End));
        }
        body = body.push(
            button(layers).padding(0).on_press(Message::OpenLink(e.url.clone())).style(|_, _| button::Style::default()),
        );
        self.edged(body.into(), rgb(YOUTUBE_RED), 432.0)
    }

    /// A post on X: who wrote it, what it says, and its picture.
    fn post_card<'a>(&'a self, e: &'a Embed) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let (name, handle) = split_author(e.author.as_deref().unwrap_or("Post on X"));
        let profile = e.author_url.clone().unwrap_or_else(|| e.url.clone());
        let initial = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        let face = container(text(initial).size(fs).font(bold()).color(Color::WHITE))
            .center(36)
            .style(style::flat(Color::from_rgb8(0x1D, 0x9B, 0xF0), 18.0));
        let mut who = column![self.link_text(name.to_string(), &profile, fs * 0.95, t.header.0)].spacing(1);
        if let Some(h) = handle {
            who = who.push(text(h.to_string()).size(fs * 0.8).color(t.muted.0));
        }
        let logo = container(text("𝕏").size(fs * 1.1).font(bold()).color(t.header.0)).padding([0, 4]);
        let mut body =
            column![row![face, who.width(Length::Fill), logo].spacing(10).align_y(Alignment::Center)].spacing(10);
        if let Some(desc) = e.description.as_ref().or(e.title.as_ref()) {
            body = body.push(text(desc.clone()).size(fs * 0.95).color(t.text.0));
        }
        if let Some(img) = &e.image {
            let (w, h) = media::fit(img.width.unwrap_or(16), img.height.unwrap_or(9), 400.0, 300.0);
            body = body.push(
                button(self.embed_thumb(e, w, h, false))
                    .padding(0)
                    .on_press(Message::OpenViewer(img.clone()))
                    .style(|_, _| button::Style::default()),
            );
        }
        body = body.push(
            button(text("View on X").size(fs * 0.8).color(t.muted.0))
                .padding(0)
                .on_press(Message::OpenLink(e.url.clone()))
                .style(style::link_button(t.muted.0, t.header.0)),
        );
        let edge = t.hover.0;
        let floating = style::darken(t.chat.0, 0.18);
        container(body)
            .padding([12, 14])
            .width(432)
            .style(move |_| container::Style {
                background: Some(floating.into()),
                border: Border { radius: 12.0.into(), width: 1.0, color: edge },
                ..Default::default()
            })
            .into()
    }

    /// A song or album on Spotify: artwork, title, artist and length.
    fn spotify_card<'a>(&'a self, e: &'a Embed) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let green = rgb(SPOTIFY_GREEN);
        let mut info = column![
            row![
                container(space()).width(10).height(10).style(style::flat(green, 5.0)),
                text("Spotify").size(fs * 0.75).font(bold()).color(green)
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(4)
        .width(Length::Fill);
        if let Some(title) = &e.title {
            info = info.push(self.link_text(title.clone(), &e.url, fs * 1.0, t.header.0));
        }
        if let Some(artist) = &e.author {
            info = info.push(match &e.author_url {
                Some(url) => self.link_text(artist.clone(), url, fs * 0.85, t.text.0),
                None => text(artist.clone()).size(fs * 0.85).color(t.text.0).into(),
            });
        }
        if let Some(secs) = e.duration_secs.filter(|s| *s > 0) {
            info = info.push(text(clock(secs)).size(fs * 0.75).color(t.muted.0));
        }
        let play = button(container(icons::icon(&icons::PLAY, 18.0, Color::BLACK)).center(40))
            .padding(0)
            .on_press(Message::OpenLink(e.url.clone()))
            .style(move |_, status| button::Style {
                background: Some(
                    if matches!(status, button::Status::Hovered) { style::lighten(green, 0.15) } else { green }.into(),
                ),
                border: Border { radius: 20.0.into(), ..Default::default() },
                ..Default::default()
            });
        let art = button(self.embed_thumb(e, 80.0, 80.0, true))
            .padding(0)
            .on_press(Message::OpenLink(e.url.clone()))
            .style(|_, _| button::Style::default());
        let body = row![art, info, play].spacing(14).align_y(Alignment::Center);
        self.edged(body.into(), green, 432.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_preview_text() {
        assert_eq!(clock(213), "3:33");
        assert_eq!(clock(3723), "1:02:03");
        assert_eq!(split_author("Rust Lang (@rustlang)"), ("Rust Lang", Some("@rustlang")));
        assert_eq!(split_author("Just a name"), ("Just a name", None));
    }
}
