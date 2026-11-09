//! Message text (links, mentions, emoji) and link previews.

use iced::widget::{button, column, container, rich_text, row, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Padding};
use syncord_proto::{Embed, Message as ChatMessage};

use crate::app::{App, Message, Session};
use crate::icons;
use crate::media;
use crate::style::{self, EMOJI_FONT, bold, card, semibold};
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
pub fn link_end(s: &str) -> usize {
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
    /// Message text with clickable links and @mentions, and emoji in colour (and Markdown).
    pub(crate) fn message_text<'a>(&'a self, s: &'a Session, content: &'a str, size: f32) -> Element<'a, Message> {
        let t = &self.theme;
        if crate::markdown::might_have_markdown(content) {
            return self.markdown_text(s, content, size);
        }
        let parts = pieces(content);
        if parts.iter().all(|p| matches!(p, Piece::Text(_))) {
            return text(content).size(size).color(t.text.0).into();
        }
        let spans: Vec<iced::widget::text::Span<'a, Message, Font>> =
            parts.into_iter().map(|p| self.piece_span(s, p)).collect();
        rich_text(spans).size(size).color(t.text.0).on_link_click(|m| m).into()
    }

    /// One piece of message text: plain, emoji, a link or a mention pill.
    pub(crate) fn piece_span<'a>(
        &'a self,
        s: &'a Session,
        p: Piece<'_>,
    ) -> iced::widget::text::Span<'a, Message, Font> {
        let t = &self.theme;
        let link_color = Color::from_rgb8(0x00, 0xA8, 0xFC);
        let pill = |label: String, color: Color| {
            span(label)
                .color(color)
                .font(style::semibold())
                .background(with_alpha(color, 0.18))
                .border(Border { radius: 3.0.into(), ..Default::default() })
                .padding(Padding { left: 2.0, right: 2.0, ..Padding::ZERO })
        };
        let mention_color = with_alpha(Color::from_rgb8(0xC9, 0xCD, 0xFB), 1.0);
        match p {
            Piece::Text(s) => span(s.to_string()),
            Piece::Emoji(e) => span(e.to_string()).font(EMOJI_FONT),
            Piece::Link(url) => span(url.to_string()).color(link_color).link(Message::OpenLink(url.to_string())),
            Piece::User(id) => match s.users.get(&id) {
                Some(u) => pill(format!("@{}", u.name()), mention_color).link(Message::ShowProfile(id)),
                None => pill("@unknown-user".into(), mention_color),
            },
            Piece::Role(id) => match s.role(id) {
                Some(r) => pill(format!("@{}", r.name), r.color.map(rgb).unwrap_or(mention_color)),
                None => pill("@deleted-role".into(), t.muted.0),
            },
            Piece::Everyone(e) => pill(e.to_string(), mention_color),
        }
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
            Some(p) => button(crate::widgets::picture(p.clone(), w, h, false, self.theme.radius * 0.6, true))
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
                button(text(title.clone()).size(fs * 0.95).font(semibold()).color(link))
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

    /// The note a call in a direct message leaves: started / lasted / missed.
    pub(crate) fn call_note<'a>(&'a self, s: &'a Session, m: &'a ChatMessage) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let syncord_proto::MessageKind::Call { participants, ended_at } = &m.kind else { return text("").into() };
        let name = |id: i64| s.users.get(&id).map(|u| u.name().to_string()).unwrap_or_else(|| "Someone".into());
        let mine = m.author_id == s.me;
        let answered = participants.len() > 1;
        let (line, missed) = match ended_at {
            None => (format!("{} started a call.", if mine { "You".to_string() } else { name(m.author_id) }), false),
            Some(end) => {
                let length = describe_length((*end - m.created_at).max(0) as u64);
                match (answered, mine) {
                    (true, _) => (
                        format!(
                            "{} started a call that lasted {length}.",
                            if mine { "You".to_string() } else { name(m.author_id) }
                        ),
                        false,
                    ),
                    (false, true) => ("You started a call that nobody answered.".to_string(), false),
                    (false, false) => (format!("You missed a call from {}.", name(m.author_id)), true),
                }
            }
        };
        let color = if missed { t.danger.0 } else { t.online.0 };
        let mut r = row![
            icons::icon(&icons::PHONE, fs * 1.2, color),
            text(line).size(fs * 0.95).color(t.text.0),
            text(crate::view::short_time_pub(m.created_at)).size(fs * 0.75).color(t.muted.0),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let in_it = self.voice.as_ref().is_some_and(|v| v.channel_id == m.channel_id);
        if ended_at.is_none() && !in_it {
            r = r.push(
                button(text("Join Call").size(fs * 0.85))
                    .padding([4, 12])
                    .on_press(Message::JoinVoice(m.channel_id))
                    .style(style::solid_button(t.online.0, t.radius * 0.6)),
            );
        }
        container(r).padding(Padding { top: 6.0, bottom: 6.0, left: 30.0, right: 16.0 }).into()
    }

    /// "So-and-so pinned a message to this channel. See all pinned messages."
    pub(crate) fn pin_note<'a>(&'a self, s: &'a Session, m: &'a ChatMessage) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let syncord_proto::MessageKind::Pinned { message_id } = m.kind else { return text("").into() };
        let who = if m.author_id == s.me {
            "You".to_string()
        } else {
            s.users.get(&m.author_id).map(|u| u.name().to_string()).unwrap_or_else(|| "Someone".into())
        };
        let link = |label: &'static str, msg: Message| {
            button(text(label).size(fs * 0.95).font(style::semibold()))
                .padding(0)
                .on_press(msg)
                .style(style::link_button(t.header.0, t.accent.0))
        };
        let r = row![
            icons::icon(&icons::PIN, fs * 1.2, t.muted.0),
            text(format!("{who} pinned")).size(fs * 0.95).color(t.text.0),
            link("a message", Message::OpenSearchResult(m.channel_id, message_id)),
            text("to this channel.").size(fs * 0.95).color(t.text.0),
            link("See all pinned messages.", Message::TogglePins),
            text(crate::view::short_time_pub(m.created_at)).size(fs * 0.75).color(t.muted.0),
        ]
        .spacing(5)
        .align_y(Alignment::Center);
        container(r).padding(Padding { top: 6.0, bottom: 6.0, left: 30.0, right: 16.0 }).into()
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
    text("").font(bold()).into()
}

/// "a few seconds", "1 minute", "12 minutes", "2 hours".
pub fn describe_length(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "a few seconds".into(),
        1 => "a minute".into(),
        2..=59 => format!("{minutes} minutes"),
        60..=119 => "an hour".into(),
        _ => format!("{} hours", minutes / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::{Piece, pieces};

    #[test]
    fn call_lengths() {
        assert_eq!(super::describe_length(20_000), "a few seconds");
        assert_eq!(super::describe_length(12 * 60_000 + 5), "12 minutes");
        assert_eq!(super::describe_length(3 * 3_600_000), "3 hours");
    }

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
