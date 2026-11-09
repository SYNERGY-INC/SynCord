//! Searching messages (Ctrl+F): the search box, the results panel, and jumping
//! to a result (which loads the messages around it, like Discord).

use std::time::Duration;

use chrono::TimeZone;

use celerity_proto::{ChannelId, ChannelKind, ClientMsg, Message as ChatMessage, MessageId, SearchHas, SearchQuery};
use iced::widget::{Id, button, column, container, operation, row, scrollable, space, text, text_input};
use iced::{Alignment, Color, Element, Length, Padding, Task};

use crate::app::{App, Message, Session};
use crate::style::{self, bold, card, semibold};
use crate::theme::with_alpha;

pub const SEARCH_ID: &str = "search";
/// How long a message you jumped to stays highlighted.
const HIGHLIGHT_FOR: Duration = Duration::from_secs(3);

/// The pinned messages panel: whose pins, and the list once it arrives.
pub struct Pins {
    pub channel: ChannelId,
    pub messages: Option<Vec<ChatMessage>>,
}

#[derive(Default)]
pub struct Search {
    /// What's typed in the search box.
    pub input: String,
    /// The results panel is showing.
    pub open: bool,
    /// The query the results are for (first page).
    pub sent: Option<SearchQuery>,
    pub results: Vec<ChatMessage>,
    pub more: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// The message just jumped to.
    pub highlight: Option<MessageId>,
}

/// Split on spaces, keeping "quoted bits" together (for `from:"Big Bob"`).
fn tokens(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Turn "from:alice in:general has:image pizza" into a query. Searches the
/// conversation you're in when it's a DM and no channel is given.
pub fn parse(input: &str, s: &Session) -> Result<SearchQuery, String> {
    let mut q = SearchQuery::default();
    for tok in tokens(input) {
        let (key, value) = match tok.split_once(':') {
            Some((k, v)) if !v.is_empty() && ["from", "in", "has"].contains(&k.to_lowercase().as_str()) => {
                (k.to_lowercase(), v.to_string())
            }
            _ => {
                q.words.push(tok);
                continue;
            }
        };
        match key.as_str() {
            "from" => {
                let name = value.trim_start_matches('@').to_lowercase();
                let user = s
                    .users
                    .values()
                    .find(|u| u.username.to_lowercase() == name)
                    .or_else(|| s.users.values().find(|u| u.name().to_lowercase() == name))
                    .ok_or_else(|| format!("Nobody here is called \"{value}\"."))?;
                q.from = Some(user.id);
            }
            "in" => {
                let name = value.trim_start_matches('#').to_lowercase();
                let ch = s
                    .channels
                    .iter()
                    .find(|c| c.kind == ChannelKind::Text && c.name.to_lowercase() == name)
                    .ok_or_else(|| format!("There's no text channel called #{}.", value.trim_start_matches('#')))?;
                q.channel_id = Some(ch.id);
            }
            _ => {
                q.has = Some(match value.to_lowercase().trim_end_matches('s') {
                    "link" => SearchHas::Link,
                    "file" => SearchHas::File,
                    "image" => SearchHas::Image,
                    "video" => SearchHas::Video,
                    _ => return Err("has: can be link, file, image or video.".into()),
                });
            }
        }
    }
    if q.channel_id.is_none()
        && let Some(id) = s.current.filter(|c| s.direct(*c).is_some())
    {
        q.channel_id = Some(id);
    }
    Ok(q)
}

/// Same search, ignoring which page.
fn same(a: &SearchQuery, b: &SearchQuery) -> bool {
    SearchQuery { before: None, ..a.clone() } == SearchQuery { before: None, ..b.clone() }
}

impl App {
    pub(crate) fn update_search(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TogglePins => {
                if self.pins.take().is_none()
                    && let Some(channel) = self.session.as_ref().and_then(|s| s.current)
                {
                    self.search.open = false;
                    self.pins = Some(Pins { channel, messages: None });
                    self.send(ClientMsg::FetchPins { channel_id: channel });
                }
            }
            Message::MenuPin(id, pinned) => {
                self.message_menu = None;
                self.send(ClientMsg::SetPinned { message_id: id, pinned });
            }
            Message::OpenSearch => {
                self.pins = None;
                self.search.open = true;
                return operation::focus(Id::new(SEARCH_ID));
            }
            Message::SearchInput(text) => self.search.input = text,
            Message::SubmitSearch => {
                let Some(s) = &self.session else { return Task::none() };
                self.pins = None;
                self.search.open = true;
                match parse(&self.search.input, s) {
                    Ok(q) if q.words.is_empty() && q.from.is_none() && q.has.is_none() && q.channel_id.is_none() => {}
                    Ok(q) => {
                        self.search.error = None;
                        self.search.results.clear();
                        self.search.more = false;
                        self.search.loading = true;
                        self.search.sent = Some(q.clone());
                        self.send(ClientMsg::Search(q));
                        return operation::snap_to(Id::new("search-results"), operation::RelativeOffset::START);
                    }
                    Err(e) => {
                        self.search.error = Some(e);
                        self.search.results.clear();
                        self.search.sent = None;
                    }
                }
            }
            Message::SearchMore => {
                if let Some(q) = &self.search.sent
                    && !self.search.loading
                    && let Some(last) = self.search.results.last()
                {
                    let q = SearchQuery { before: Some(last.id), ..q.clone() };
                    self.search.loading = true;
                    self.send(ClientMsg::Search(q));
                }
            }
            Message::CloseSearch => {
                self.search.open = false;
            }
            Message::OpenSearchResult(channel_id, message_id) => {
                let Some(s) = &mut self.session else { return Task::none() };
                // Loading so opening the channel doesn't also fetch its newest messages.
                s.chans.entry(channel_id).or_default().loading = true;
                self.send(ClientMsg::FetchAround { channel_id, message_id });
                self.search.highlight = Some(message_id);
                return self.select_channel(channel_id);
            }
            Message::JumpToPresent => {
                if let Some(s) = &mut self.session
                    && let Some(id) = s.current
                {
                    let st = s.chans.entry(id).or_default();
                    st.loaded = false;
                    st.detached = false;
                    st.loading = true;
                    self.send(ClientMsg::FetchHistory { channel_id: id, before: None });
                }
            }
            Message::ClearHighlight(id) if self.search.highlight == Some(id) => self.search.highlight = None,
            _ => {}
        }
        Task::none()
    }

    pub(crate) fn on_search_results(&mut self, query: SearchQuery, results: Vec<ChatMessage>, more: bool) {
        let Some(sent) = &self.search.sent else { return };
        if !same(sent, &query) {
            return; // an older search
        }
        if query.before.is_none() {
            self.search.results = results;
        } else {
            self.search.results.extend(results);
        }
        self.search.more = more;
        self.search.loading = false;
    }

    /// The messages around one you jumped to arrived: show them instead of the newest.
    pub(crate) fn on_around(
        &mut self,
        channel_id: ChannelId,
        message_id: MessageId,
        messages: Vec<ChatMessage>,
        reached_start: bool,
    ) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        let after = messages.iter().filter(|m| m.id > message_id).count();
        let newest = messages.last().map(|m| m.id);
        // Not showing the newest messages: new ones aren't added until "Jump to present".
        let detached = match s.latest.get(&channel_id) {
            Some(latest) => newest < Some(*latest),
            None => after >= 5,
        };
        let st = s.chans.entry(channel_id).or_default();
        st.messages = messages;
        st.loaded = true;
        st.loading = false;
        st.reached_start = reached_start;
        st.detached = detached;
        if s.current != Some(channel_id) {
            return Task::none();
        }
        let clear = Task::perform(tokio::time::sleep(HIGHLIGHT_FOR), move |_| Message::ClearHighlight(message_id));
        // The jumped-to message is a few from the end, so it shows at the bottom.
        let snap = operation::snap_to(Id::new(crate::app::MESSAGES_ID), operation::RelativeOffset::START);
        Task::batch([snap, clear, self.request_pictures()])
    }

    /// The search box in the channel header.
    pub(crate) fn search_box(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        text_input("Search", &self.search.input)
            .id(Id::new(SEARCH_ID))
            .on_input(Message::SearchInput)
            .on_submit(Message::SubmitSearch)
            .padding([6, 14])
            .size(fs * 0.85)
            .width(if self.search.open { 240 } else { 170 })
            .style(style::pill_input(t, style::mix(t.chat.0, t.header.0, 0.06)))
            .into()
    }

    /// Where a result was said: "#general" or "@bob".
    fn place(s: &Session, channel: ChannelId) -> String {
        if let Some(c) = s.channels.iter().find(|c| c.id == channel) {
            return format!("#{}", c.name);
        }
        s.direct(channel)
            .and_then(|d| s.users.get(&d.user_id))
            .map(|u| format!("@{}", u.name()))
            .unwrap_or_else(|| "Unknown conversation".into())
    }

    /// The panel on the right listing results.
    pub(crate) fn search_panel<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let sr = &self.search;
        let title = if sr.loading && sr.results.is_empty() {
            "Searching...".to_string()
        } else if sr.sent.is_none() {
            "Search".to_string()
        } else {
            match (sr.results.len(), sr.more) {
                (0, _) => "No results".into(),
                (1, false) => "1 result".into(),
                (n, false) => format!("{n} results"),
                (n, true) => format!("{n}+ results"),
            }
        };
        let header = row![
            text(title).size(fs * 0.95).font(bold()).color(t.header.0).width(Length::Fill),
            button(crate::icons::icon(&crate::icons::CLOSE, fs, t.muted.0))
                .padding(4)
                .on_press(Message::CloseSearch)
                .style(style::icon_button(t)),
        ]
        .align_y(Alignment::Center);

        let mut list = column![].spacing(8).padding(Padding { top: 0.0, bottom: 12.0, left: 12.0, right: 12.0 });
        if let Some(e) = &sr.error {
            list = list.push(text(e.clone()).size(fs * 0.85).color(t.danger.0));
        }
        if sr.sent.is_none() || (sr.results.is_empty() && !sr.loading) {
            let tips = [
                ("from:", "a person, like from:alice"),
                ("in:", "a channel, like in:general"),
                ("has:", "link, file, image or video"),
            ];
            let mut col = column![text("Narrow it down").size(fs * 0.75).font(bold()).color(t.muted.0)].spacing(6);
            for (k, v) in tips {
                col = col.push(
                    row![
                        text(k).size(fs * 0.85).font(semibold()).color(t.header.0),
                        text(v).size(fs * 0.85).color(t.muted.0)
                    ]
                    .spacing(6),
                );
            }
            list = list.push(col);
        }
        for m in &sr.results {
            list = list.push(self.message_card(s, m, None));
        }
        if sr.more || (sr.loading && !sr.results.is_empty()) {
            let label = if sr.loading { "Loading..." } else { "Show more" };
            list = list.push(
                container(
                    button(text(label).size(fs * 0.85))
                        .padding([4, 10])
                        .on_press_maybe((!sr.loading).then_some(Message::SearchMore))
                        .style(style::icon_button(t)),
                )
                .center_x(Length::Fill),
            );
        }
        container(
            column![
                container(header).padding([12, 12]),
                scrollable(list).id(Id::new("search-results")).height(Length::Fill).style(style::scroll(t)),
            ]
            .spacing(4),
        )
        .width(340)
        .height(Length::Fill)
        .into()
    }

    /// A message shown on its own (search results, pins): click to jump to it.
    /// `extra` goes in the top corner (the Unpin button).
    pub(crate) fn message_card<'a>(
        &'a self,
        s: &'a Session,
        m: &'a ChatMessage,
        extra: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let floating = t.floating.0;
        let author = s.users.get(&m.author_id);
        let name = author.map(|u| u.name()).unwrap_or("Unknown user");
        let mut snippet = crate::view::truncate(&self.decode_mentions(&m.content).0, 300);
        if snippet.is_empty() {
            snippet = match m.attachments.len() {
                0 => "(no text)".into(),
                1 => format!("📎 {}", m.attachments[0].filename),
                n => format!("📎 {n} files"),
            };
        }
        let avatar: Element<'a, Message> = match author {
            Some(u) => self.avatar(u, 32.0),
            None => space().width(32).into(),
        };
        let mut top = row![
            text(Self::place(s, m.channel_id)).size(fs * 0.75).font(semibold()).color(t.muted.0).width(Length::Fill)
        ]
        .align_y(Alignment::Center);
        if let Some(extra) = extra {
            top = top.push(extra);
        }
        let card_body = column![
            top,
            row![
                avatar,
                column![
                    row![
                        text(name.to_string()).size(fs * 0.9).font(semibold()).color(t.header.0),
                        text(crate::view::long_time(m.created_at)).size(fs * 0.7).color(t.muted.0),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    text(snippet).size(fs * 0.9).color(t.text.0),
                ]
                .spacing(2)
                .width(Length::Fill),
            ]
            .spacing(10),
        ]
        .spacing(6);
        button(card_body)
            .padding(10)
            .width(Length::Fill)
            .on_press(Message::OpenSearchResult(m.channel_id, m.id))
            .style(move |_, status| button::Style {
                background: Some(
                    match status {
                        button::Status::Hovered | button::Status::Pressed => style::darken(floating, 0.15),
                        _ => floating,
                    }
                    .into(),
                ),
                border: iced::Border { radius: 8.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into()
    }

    /// The pinned messages panel (in place of the member list).
    pub(crate) fn pins_panel<'a>(&'a self, s: &'a Session, pins: &'a Pins) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let can_unpin = s.direct(pins.channel).is_some() || s.can(celerity_proto::perms::MANAGE_MESSAGES);
        let header = row![
            text("Pinned Messages").size(fs * 0.95).font(bold()).color(t.header.0).width(Length::Fill),
            button(crate::icons::icon(&crate::icons::CLOSE, fs, t.muted.0))
                .padding(4)
                .on_press(Message::TogglePins)
                .style(style::icon_button(t)),
        ]
        .align_y(Alignment::Center);
        let mut list = column![].spacing(8).padding(Padding { top: 0.0, bottom: 12.0, left: 12.0, right: 12.0 });
        match &pins.messages {
            None => list = list.push(text("Loading...").size(fs * 0.85).color(t.muted.0)),
            Some(ms) if ms.is_empty() => {
                list = list.push(
                    text("Nothing's pinned here yet. Right-click a message and choose Pin Message.")
                        .size(fs * 0.85)
                        .color(t.muted.0),
                )
            }
            Some(ms) => {
                for m in ms {
                    let unpin = can_unpin.then(|| {
                        Element::from(
                            button(text("Unpin").size(fs * 0.75))
                                .padding([2, 8])
                                .on_press(Message::MenuPin(m.id, false))
                                .style(style::icon_button(t)),
                        )
                    });
                    list = list.push(self.message_card(s, m, unpin));
                }
            }
        }
        container(
            column![
                container(header).padding([12, 12]),
                scrollable(list).height(Length::Fill).style(style::scroll(t)),
            ]
            .spacing(4),
        )
        .width(340)
        .height(Length::Fill)
        .into()
    }

    /// "N new messages since 3:42 PM" over a channel opened with unread messages: jump to
    /// the first one, or mark them read.
    pub(crate) fn unread_bar<'a>(&'a self, s: &'a Session, channel: ChannelId) -> Option<Element<'a, Message>> {
        let (ch, since) = self.new_since.filter(|_| self.unread_bar)?;
        if ch != channel {
            return None;
        }
        let messages = &s.chans.get(&channel)?.messages;
        let new: Vec<_> = messages.iter().filter(|m| m.id > since && m.author_id != s.me).collect();
        let first = new.first()?;
        // A couple of new messages are all on screen anyway.
        if new.len() < 4 {
            return None;
        }
        let t = &self.theme;
        let fs = self.fs();
        let accent = t.accent.0;
        let when = chrono::Local
            .timestamp_millis_opt(first.created_at)
            .single()
            .map(|d| d.format("%-I:%M %p").to_string())
            .unwrap_or_default();
        let count = if new.len() >= messages.len() { format!("{}+", new.len()) } else { new.len().to_string() };
        Some(
            container(
                row![
                    button(
                        text(format!("{count} new messages since {when}"))
                            .size(fs * 0.85)
                            .font(semibold())
                            .color(Color::WHITE)
                    )
                    .padding(0)
                    .width(Length::Fill)
                    .on_press(Message::JumpToUnread)
                    .style(|_, _| iced::widget::button::Style::default()),
                    button(text("Mark as read").size(fs * 0.85).font(semibold()).color(Color::WHITE))
                        .padding([3, 10])
                        .on_press(Message::DismissUnread)
                        .style(style::solid_button(style::darken(accent, 0.25), 6.0)),
                ]
                .align_y(Alignment::Center),
            )
            .padding([4, 16])
            .width(Length::Fill)
            .style(card(with_alpha(accent, 0.9), 0.0))
            .into(),
        )
    }

    /// "You're viewing older messages" bar, while not showing the newest.
    pub(crate) fn older_bar(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let accent = t.accent.0;
        container(
            row![
                text("You're viewing older messages").size(fs * 0.85).color(Color::WHITE).width(Length::Fill),
                button(text("Jump to present").size(fs * 0.85).font(semibold()))
                    .padding([3, 10])
                    .on_press(Message::JumpToPresent)
                    .style(style::solid_button(style::darken(accent, 0.25), 6.0)),
            ]
            .align_y(Alignment::Center),
        )
        .padding([4, 16])
        .width(Length::Fill)
        .style(card(with_alpha(accent, 0.9), 0.0))
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::tokens;

    #[test]
    fn splitting_queries() {
        assert_eq!(tokens("  pizza  from:alice "), ["pizza", "from:alice"]);
        assert_eq!(tokens(r#"from:"Big Bob" has:image"#), ["from:Big Bob", "has:image"]);
        assert!(tokens("   ").is_empty());
    }
}
