//! Message text (links, mentions, emoji) and link previews.

use iced::widget::{button, column, container, image, rich_text, row, space, span, text};
use iced::{Alignment, Border, Color, ContentFit, Element, Font, Length, Padding};
use syncord_proto::{Embed, Message as ChatMessage};

use crate::app::{App, Message, Session};
use crate::media;
use crate::style::{self, BOLD, EMOJI_FONT, SEMIBOLD, card};
use crate::theme::{rgb, with_alpha};

/// A piece of message text.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece<'a> {
    Text(&'a str),
    Emoji(&'a str),
    Link(&'a str),
    User(i64),
    Role(i64),
    Everyone(&'a str),
}

/// Links in text: `http(s)://` up to whitespace, without trailing punctuation
/// (but keeping balanced brackets, as in Wikipedia links).
fn link_end(s: &str) -> usize {
    let mut end = s.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"').unwrap_or(s.len());
    while let Some(last) = s[..end].chars().last() {
        let trim = match last {
            '.' | ',' | '!' | '?' | ':' | ';' | '\'' | '*' | '_' | '~' => true,
            ')' => s[..end].matches('(').count() < s[..end].matches(')').count(),
            ']' => s[..end].matches('[').count() < s[..end].matches(']').count(),
            _ => false,
        };
        if !trim {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

/// Split message text into pieces to draw.
pub fn pieces<'a>(content: &'a str) -> Vec<Piece<'a>> {
    let mut out = Vec::new();
    let mut text_start = 0usize;
    let mut i = 0usize;
    let bytes = content.as_bytes();
    let push_text = |out: &mut Vec<Piece<'a>>, from: usize, to: usize| {
        if from < to {
            for (emoji, part) in style::split_emoji(&content[from..to]) {
                out.push(if emoji { Piece::Emoji(part) } else { Piece::Text(part) });
            }
        }
    };
    while i < bytes.len() {
        let here = &content[i..];
        let found: Option<(Piece<'a>, usize)> = if here.starts_with("https://") || here.starts_with("http://") {
            let end = link_end(here);
            let scheme = if here.starts_with("https://") { 8 } else { 7 };
            (end > scheme).then(|| (Piece::Link(&here[..end]), end))
        } else if let Some(after) = here.strip_prefix("<@") {
            let (role, digits) = match after.strip_prefix('&') {
                Some(d) => (true, d),
                None => (false, after),
            };
            let close = digits.find('>');
            match close.and_then(|c| digits[..c].parse::<i64>().ok().map(|id| (id, c))) {
                Some((id, c)) => {
                    let len = (here.len() - digits.len()) + c + 1;
                    Some((if role { Piece::Role(id) } else { Piece::User(id) }, len))
                }
                None => None,
            }
        } else if here.starts_with("@everyone") || here.starts_with("@here") {
            let len = if here.starts_with("@everyone") { 9 } else { 5 };
            Some((Piece::Everyone(&here[..len]), len))
        } else {
            None
        };
        match found {
            Some((piece, len)) => {
                push_text(&mut out, text_start, i);
                out.push(piece);
                i += len;
                text_start = i;
            }
            None => i += here.chars().next().map_or(1, char::len_utf8),
        }
    }
    push_text(&mut out, text_start, content.len());
    out
}

impl App {
    /// Message text with clickable links and @mentions, and emoji in colour.
    pub(crate) fn message_text<'a>(&'a self, s: &'a Session, content: &'a str, size: f32) -> Element<'a, Message> {
        let t = &self.theme;
        let parts = pieces(content);
        if parts.iter().all(|p| matches!(p, Piece::Text(_))) {
            return text(content).size(size).color(t.text.0).into();
        }
        let link_color = Color::from_rgb8(0x00, 0xA8, 0xFC);
        let pill = |label: String, color: Color| {
            span(label)
                .color(color)
                .font(Font { weight: iced::font::Weight::Semibold, ..style::INTER })
                .background(with_alpha(color, 0.18))
                .border(Border { radius: 3.0.into(), ..Default::default() })
                .padding(Padding { left: 2.0, right: 2.0, ..Padding::ZERO })
        };
        let mention_color = with_alpha(Color::from_rgb8(0xC9, 0xCD, 0xFB), 1.0);
        let spans: Vec<iced::widget::text::Span<'a, Message, Font>> = parts
            .into_iter()
            .map(|p| match p {
                Piece::Text(s) => span(s),
                Piece::Emoji(e) => span(e).font(EMOJI_FONT),
                Piece::Link(url) => span(url).color(link_color).link(Message::OpenLink(url.to_string())),
                Piece::User(id) => match s.users.get(&id) {
                    Some(u) => pill(format!("@{}", u.name()), mention_color).link(Message::ShowProfile(id)),
                    None => pill("@unknown-user".into(), mention_color),
                },
                Piece::Role(id) => match s.role(id) {
                    Some(r) => pill(format!("@{}", r.name), r.color.map(rgb).unwrap_or(mention_color)),
                    None => pill("@deleted-role".into(), t.muted.0),
                },
                Piece::Everyone(e) => pill(e.to_string(), mention_color),
            })
            .collect();
        rich_text(spans).size(size).color(t.text.0).on_link_click(|m| m).into()
    }

    /// Link previews under a message.
    pub(crate) fn embeds_view<'a>(&'a self, m: &'a ChatMessage) -> Option<Element<'a, Message>> {
        if m.embeds.is_empty() {
            return None;
        }
        let mut col = column![].spacing(6);
        for e in &m.embeds {
            col = col.push(self.embed_view(e));
        }
        Some(col.into())
    }

    fn embed_picture<'a>(&'a self, a: &'a syncord_proto::Attachment, max_w: f32, max_h: f32) -> Element<'a, Message> {
        let (w, h) = media::fit(a.width.unwrap_or(1), a.height.unwrap_or(1), max_w, max_h);
        match self.preview(a) {
            Some(p) => button(
                image(p.frame(self.animation_ms(None)).clone())
                    .width(w)
                    .height(h)
                    .content_fit(ContentFit::Contain)
                    .border_radius(self.theme.radius * 0.6),
            )
            .padding(0)
            .on_press(Message::OpenViewer(a.clone()))
            .style(|_, _| button::Style::default())
            .into(),
            None => container(space()).width(w).height(h).style(card(self.theme.floating.0, 6.0)).into(),
        }
    }

    fn embed_view<'a>(&'a self, e: &'a Embed) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        if e.media
            && let Some(img) = &e.image
        {
            return self.embed_picture(img, 400.0, 300.0);
        }
        let mut body = column![].spacing(6).width(Length::Fill);
        if let Some(site) = &e.site_name {
            body = body.push(text(site.clone()).size(fs * 0.75).color(t.muted.0));
        }
        if let Some(title) = &e.title {
            let link = Color::from_rgb8(0x00, 0xA8, 0xFC);
            body = body.push(
                button(text(title.clone()).size(fs * 0.95).font(SEMIBOLD).color(link))
                    .padding(0)
                    .on_press(Message::OpenLink(e.url.clone()))
                    .style(style::link_button(link, link)),
            );
        }
        if let Some(desc) = &e.description {
            body = body.push(text(desc.clone()).size(fs * 0.85).color(t.text.0));
        }
        if let Some(img) = &e.image {
            body = body.push(self.embed_picture(img, 380.0, 220.0));
        }
        let accent = e.color.map(rgb).unwrap_or(t.hover.0);
        let floating = style::darken(t.chat.0, 0.18);
        // The coloured left edge is the accent showing through a 4 px gap (a "fill the height"
        // bar collapses inside the scrolling message list).
        let inner = container(body).padding([10, 12]).width(Length::Fill).style(move |_| container::Style {
            background: Some(floating.into()),
            border: Border { radius: iced::border::right(4.0), ..Default::default() },
            ..Default::default()
        });
        container(inner)
            .padding(Padding { left: 4.0, ..Padding::ZERO })
            .width(Length::Fixed(432.0))
            .style(move |_| container::Style {
                background: Some(accent.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into()
    }

    /// A folded-away message from someone blocked or ignored.
    pub(crate) fn hidden_message<'a>(&'a self, s: &'a Session, m: &'a ChatMessage) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let why = if s.blocked.contains(&m.author_id) { "Blocked message" } else { "Ignored message" };
        container(
            row![
                text(why).size(fs * 0.85).color(t.muted.0),
                button(text("Show message").size(fs * 0.85))
                    .padding(0)
                    .on_press(Message::RevealMessage(m.id))
                    .style(style::link_button(t.accent.0, t.header.0)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([4, 72])
        .into()
    }
}

#[allow(dead_code)]
fn _unused(_: &App) -> Element<'_, Message> {
    text("").font(BOLD).into()
}

#[cfg(test)]
mod tests {
    use super::{Piece, pieces};

    #[test]
    fn splits_links_mentions_and_emoji() {
        let p = pieces("hi <@12> see https://example.com/a_(b). and <@&3> @everyone 🎉");
        assert_eq!(
            p,
            vec![
                Piece::Text("hi "),
                Piece::User(12),
                Piece::Text(" see "),
                Piece::Link("https://example.com/a_(b)"),
                Piece::Text(". and "),
                Piece::Role(3),
                Piece::Text(" "),
                Piece::Everyone("@everyone"),
                Piece::Text(" "),
                Piece::Emoji("🎉"),
            ]
        );
        assert_eq!(pieces("plain"), vec![Piece::Text("plain")]);
        assert_eq!(pieces("<@x> https://"), vec![Piece::Text("<@x> https://")]);
    }
}
