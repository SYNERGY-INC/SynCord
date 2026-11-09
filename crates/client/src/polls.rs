//! Polls (0.7): making one from the message box's + menu, and showing one in a message
//! as answers with bars you can click to vote.

use celerity_proto::social::{MAX_POLL_OPTION, MAX_POLL_OPTIONS, MAX_POLL_QUESTION, MIN_POLL_OPTIONS};
use celerity_proto::{ClientMsg, Message as ChatMessage, MessageId, Poll, ServerMsg, User, perms};
use iced::widget::{Id, button, checkbox, column, container, pick_list, row, space, text, text_input, tooltip};
use iced::{Alignment, Border, Element, Length, Task};

use crate::app::{App, Message, Overlay, Session};
use crate::features::Dialog;
use crate::icons;
use crate::style::{self, bold, card, label, semibold};
use crate::theme::with_alpha;

/// How long voting stays open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PollLength {
    Hour,
    FourHours,
    EightHours,
    #[default]
    Day,
    ThreeDays,
    Week,
}

impl PollLength {
    pub const ALL: [PollLength; 6] =
        [Self::Hour, Self::FourHours, Self::EightHours, Self::Day, Self::ThreeDays, Self::Week];

    pub fn secs(self) -> u64 {
        const H: u64 = 3600;
        match self {
            Self::Hour => H,
            Self::FourHours => 4 * H,
            Self::EightHours => 8 * H,
            Self::Day => 24 * H,
            Self::ThreeDays => 72 * H,
            Self::Week => 168 * H,
        }
    }
}

impl std::fmt::Display for PollLength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hour => "1 hour",
            Self::FourHours => "4 hours",
            Self::EightHours => "8 hours",
            Self::Day => "24 hours",
            Self::ThreeDays => "3 days",
            Self::Week => "1 week",
        })
    }
}

/// The poll being made.
#[derive(Debug, Clone, PartialEq)]
pub struct PollDraft {
    pub question: String,
    pub answers: Vec<String>,
    pub multiple: bool,
    pub length: PollLength,
}

impl Default for PollDraft {
    fn default() -> Self {
        Self { question: String::new(), answers: vec![String::new(); 2], multiple: false, length: PollLength::Day }
    }
}

impl PollDraft {
    /// The answers that were filled in.
    fn filled(&self) -> Vec<String> {
        self.answers.iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect()
    }

    pub fn ready(&self) -> bool {
        !self.question.trim().is_empty() && self.filled().len() >= MIN_POLL_OPTIONS
    }
}

#[derive(Debug, Clone)]
pub enum PollMsg {
    /// The + next to the message box.
    TogglePlus,
    /// "Upload a File" in the + menu.
    Upload,
    Open,
    Question(String),
    Answer(usize, String),
    AddAnswer,
    /// Enter in a box: on to answer `n` (a new one after a filled last answer; posting after an empty one).
    Next(usize),
    RemoveAnswer(usize),
    Multiple(bool),
    Length(PollLength),
    Send,
    Vote(MessageId, u32, bool),
    End(MessageId),
}

/// Share of `votes` in `total`, as a rounded percentage.
pub fn percent(votes: usize, total: usize) -> u32 {
    if total == 0 { 0 } else { ((votes as f64 * 100.0) / total as f64).round() as u32 }
}

/// "3h 20m", "2d 4h", "12m", "less than a minute".
pub fn time_left(ms: i64) -> String {
    let mins = ms / 60_000;
    let (d, h, m) = (mins / 1440, mins / 60 % 24, mins % 60);
    match (d, h, m) {
        (0, 0, 0) => "less than a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// The line under a poll: "4 votes · 3h left" or "4 votes · Final results".
pub fn footer(poll: &Poll, now: i64) -> String {
    let n = poll.voters();
    let votes = if n == 1 { "1 vote".to_string() } else { format!("{n} votes") };
    match (poll.closed, poll.ends_at) {
        (true, _) => format!("{votes} · Final results"),
        (false, Some(end)) if end > now => format!("{votes} · {} left", time_left(end - now)),
        (false, Some(_)) => format!("{votes} · Ending..."),
        (false, None) => votes,
    }
}

/// The answers with the most votes (none if nobody voted).
pub fn leaders(poll: &Poll) -> Vec<u32> {
    let most = poll.options.iter().map(|o| o.votes.len()).max().unwrap_or(0);
    if most == 0 {
        return Vec::new();
    }
    poll.options.iter().filter(|o| o.votes.len() == most).map(|o| o.id).collect()
}

impl App {
    pub(crate) fn update_polls(&mut self, msg: PollMsg) -> Task<Message> {
        match msg {
            PollMsg::TogglePlus => self.composer_plus = !self.composer_plus,
            PollMsg::Upload => {
                self.composer_plus = false;
                return self.update(Message::AttachFiles);
            }
            PollMsg::Open => {
                self.composer_plus = false;
                self.overlay = Some(Overlay::Dialog(Dialog::Poll(PollDraft::default())));
                return iced::widget::operation::focus(Id::new("poll-question"));
            }
            PollMsg::Question(q) => self.edit_poll(|p| p.question = q.chars().take(MAX_POLL_QUESTION).collect()),
            PollMsg::Answer(i, a) => self.edit_poll(|p| {
                if let Some(slot) = p.answers.get_mut(i) {
                    *slot = a.chars().take(MAX_POLL_OPTION).collect();
                }
            }),
            PollMsg::AddAnswer => {
                self.edit_poll(|p| {
                    if p.answers.len() < MAX_POLL_OPTIONS {
                        p.answers.push(String::new());
                    }
                });
                let last = match &self.overlay {
                    Some(Overlay::Dialog(Dialog::Poll(p))) => p.answers.len() - 1,
                    _ => 0,
                };
                return iced::widget::operation::focus(Id::from(format!("poll-answer-{last}")));
            }
            PollMsg::Next(n) => {
                let Some(Overlay::Dialog(Dialog::Poll(p))) = &self.overlay else { return Task::none() };
                if n < p.answers.len() {
                    return iced::widget::operation::focus(Id::from(format!("poll-answer-{n}")));
                }
                let last_filled = p.answers.last().is_some_and(|a| !a.trim().is_empty());
                let next =
                    if last_filled && p.answers.len() < MAX_POLL_OPTIONS { PollMsg::AddAnswer } else { PollMsg::Send };
                return self.update_polls(next);
            }
            PollMsg::RemoveAnswer(i) => self.edit_poll(|p| {
                if p.answers.len() > MIN_POLL_OPTIONS && i < p.answers.len() {
                    p.answers.remove(i);
                }
            }),
            PollMsg::Multiple(on) => self.edit_poll(|p| p.multiple = on),
            PollMsg::Length(l) => self.edit_poll(|p| p.length = l),
            PollMsg::Send => {
                let Some(Overlay::Dialog(Dialog::Poll(p))) = self.overlay.clone() else { return Task::none() };
                let Some(channel_id) = self.session.as_ref().and_then(|s| s.current) else { return Task::none() };
                if !p.ready() {
                    return Task::none();
                }
                self.send(ClientMsg::SendPoll {
                    channel_id,
                    question: p.question.trim().to_string(),
                    options: p.filled(),
                    multiple: p.multiple,
                    duration_secs: Some(p.length.secs()),
                });
                self.close_overlay();
                return iced::widget::operation::snap_to(
                    Id::new(crate::app::MESSAGES_ID),
                    iced::widget::operation::RelativeOffset::START,
                );
            }
            PollMsg::Vote(message_id, option, voted) => self.send(ClientMsg::Vote { message_id, option, voted }),
            PollMsg::End(message_id) => self.send(ClientMsg::EndPoll { message_id }),
        }
        Task::none()
    }

    fn edit_poll(&mut self, f: impl FnOnce(&mut PollDraft)) {
        if let Some(Overlay::Dialog(Dialog::Poll(p))) = &mut self.overlay {
            f(p);
        }
    }

    pub(crate) fn on_poll_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let ServerMsg::PollUpdated { channel_id, message_id, poll } = msg else { return Task::none() };
        if let Some(m) = self
            .session
            .as_mut()
            .and_then(|s| s.chans.get_mut(&channel_id))
            .and_then(|st| st.messages.iter_mut().find(|m| m.id == message_id))
        {
            m.poll = Some(poll);
        }
        Task::none()
    }

    /// A poll in a message: its answers as bars to vote with.
    pub(crate) fn poll_view<'a>(&'a self, s: &'a Session, m: &'a ChatMessage, poll: &'a Poll) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        const WIDTH: f32 = 440.0;
        let total = poll.voters();
        let now = crate::app::now_ms() + s.clock_offset_ms;
        let open = !poll.closed && poll.ends_at.is_none_or(|e| e > now);
        let winners = if poll.closed { leaders(poll) } else { Vec::new() };
        let hint = match (open, poll.multiple) {
            (false, _) => "Voting has ended",
            (true, true) => "Select one or more answers",
            (true, false) => "Select one answer",
        };
        let mut col = column![
            text(poll.question.clone()).size(fs * 1.1).font(bold()).color(t.header.0),
            text(hint).size(fs * 0.8).color(t.muted.0),
        ]
        .spacing(8);
        for o in &poll.options {
            let mine = o.votes.contains(&s.me);
            let won = winners.contains(&o.id);
            let share = percent(o.votes.len(), total);
            let fraction = if total == 0 { 0.0 } else { o.votes.len() as f32 / total as f32 };
            let (accent, online) = (t.accent.0, t.online.0);
            let fill_color = if won {
                with_alpha(online, 0.35)
            } else if mine {
                with_alpha(accent, 0.4)
            } else {
                t.hover.0
            };
            let mark = if mine { "✓ " } else { "" };
            let line = row![
                text(format!("{mark}{}", o.text))
                    .size(fs * 0.95)
                    .font(if mine { semibold() } else { style::INTER })
                    .color(t.header.0)
                    .width(Length::Fill),
                text(format!("{} · {share}%", o.votes.len())).size(fs * 0.8).color(t.text.0),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            // The bar is the answer's background, filled as far as its share of the votes.
            let filled = iced::Background::Gradient(iced::Gradient::Linear(
                iced::gradient::Linear::new(iced::Radians(std::f32::consts::FRAC_PI_2))
                    .add_stop(0.0, fill_color)
                    .add_stop(fraction, fill_color)
                    .add_stop((fraction + 0.002).min(1.0), iced::Color::TRANSPARENT)
                    .add_stop(1.0, iced::Color::TRANSPARENT),
            ));
            let content = container(line)
                .padding([0, 12])
                .width(Length::Fill)
                .height(Length::Fill)
                .align_y(Alignment::Center)
                .style(move |_| container::Style {
                    background: match fraction {
                        f if f <= 0.0 => None,
                        f if f >= 0.999 => Some(fill_color.into()),
                        _ => Some(filled),
                    },
                    border: Border { radius: 7.0.into(), ..Default::default() },
                    ..Default::default()
                });
            let edge = if mine {
                accent
            } else if won {
                online
            } else {
                t.hover.0
            };
            let vote = open.then_some(Message::Polls(PollMsg::Vote(m.id, o.id, !mine)));
            let answer = button(content).width(WIDTH).height(fs * 2.6).padding(0).on_press_maybe(vote).style(
                move |_, status| button::Style {
                    background: Some(
                        if matches!(status, button::Status::Hovered) {
                            with_alpha(edge, 0.12)
                        } else {
                            iced::Color::TRANSPARENT
                        }
                        .into(),
                    ),
                    border: Border { radius: 8.0.into(), width: 1.5, color: edge },
                    ..Default::default()
                },
            );
            let names: Vec<&str> = o.votes.iter().filter_map(|id| s.users.get(id).map(User::name)).collect();
            let who = if names.is_empty() {
                "No votes yet".to_string()
            } else if names.len() > 12 {
                format!("{} and {} more", names[..12].join(", "), names.len() - 12)
            } else {
                names.join(", ")
            };
            col = col.push(tooltip(
                answer,
                container(text(who).size(fs * 0.8)).padding([4, 8]).max_width(300).style(card(t.rail.0, 6.0)),
                tooltip::Position::Top,
            ));
        }
        let mut foot =
            row![text(footer(poll, now)).size(fs * 0.8).color(t.muted.0)].spacing(10).align_y(Alignment::Center);
        let in_server = s.channels.iter().any(|c| c.id == m.channel_id);
        let may_end = m.author_id == s.me || (in_server && s.can(perms::MANAGE_MESSAGES));
        if open && may_end {
            foot = foot.push(
                button(text("End Poll").size(fs * 0.8))
                    .padding(0)
                    .on_press(Message::Polls(PollMsg::End(m.id)))
                    .style(style::link_button(t.accent.0, t.header.0)),
            );
        }
        col = col.push(foot);
        container(col)
            .padding(14)
            .width(WIDTH + 28.0)
            .style(move |_| container::Style {
                background: Some(style::darken(t.chat.0, 0.18).into()),
                border: Border { radius: t.radius.into(), width: 1.0, color: t.hover.0 },
                ..Default::default()
            })
            .into()
    }

    /// The + menu above the message box: send a file, or make a poll.
    pub(crate) fn composer_plus_menu<'a>(
        &'a self,
        s: &Session,
        ch: &crate::app::ChannelInfo<'_>,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let item =
            |glyph: &'static std::sync::LazyLock<iced::widget::svg::Handle>, label: &'static str, msg: Message| {
                button(
                    row![icons::icon(glyph, fs * 1.2, t.muted.0), text(label).size(fs * 0.9)]
                        .spacing(10)
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([7, 10])
                .on_press(msg)
                .style(style::row_button(t, false))
            };
        let mut col = column![item(&icons::ATTACH, "Upload a File", Message::Polls(PollMsg::Upload))].spacing(2);
        // Polls work in server channels and groups, on servers that have them.
        if s.v07 && ch.direct_with.is_none() {
            col = col.push(item(&icons::POLL, "Create Poll", Message::Polls(PollMsg::Open)));
        }
        let (fill, edge, radius) = (t.floating.0, t.hover.0, t.radius);
        container(col)
            .padding(6)
            .width(220)
            .style(move |_| container::Style {
                background: Some(fill.into()),
                border: Border { radius: radius.into(), width: 1.0, color: edge },
                ..Default::default()
            })
            .into()
    }

    pub(crate) fn poll_dialog<'a>(&'a self, p: &'a PollDraft) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let pm = Message::Polls;
        let mut answers = column![].spacing(6);
        for (i, a) in p.answers.iter().enumerate() {
            let mut line = row![
                text_input(&format!("Answer {}", i + 1), a)
                    .id(Id::from(format!("poll-answer-{i}")))
                    .on_input(move |v| pm(PollMsg::Answer(i, v)))
                    .on_submit(pm(PollMsg::Next(i + 1)))
                    .padding(9)
                    .size(fs)
                    .style(style::input(t, t.rail.0))
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            if p.answers.len() > MIN_POLL_OPTIONS {
                line = line.push(
                    button(icons::icon(&icons::CLOSE, fs * 0.9, t.muted.0))
                        .padding(6)
                        .on_press(pm(PollMsg::RemoveAnswer(i)))
                        .style(style::icon_button(t)),
                );
            }
            answers = answers.push(line);
        }
        if p.answers.len() < MAX_POLL_OPTIONS {
            answers = answers.push(
                button(text("+ Add another answer").size(fs * 0.9))
                    .padding([4, 0])
                    .on_press(pm(PollMsg::AddAnswer))
                    .style(style::link_button(t.accent.0, t.header.0)),
            );
        }
        let body = column![
            row![
                icons::icon(&icons::POLL, fs * 1.4, t.accent.0),
                text("Create a poll").size(fs * 1.3).font(bold()).color(t.header.0)
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            label(t, "Question", fs),
            text_input("What do you want to ask?", &p.question)
                .id(Id::new("poll-question"))
                .on_input(move |v| pm(PollMsg::Question(v)))
                .on_submit(pm(PollMsg::Next(0)))
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
            label(t, "Answers", fs),
            answers,
            row![
                column![
                    label(t, "Voting stays open for", fs),
                    pick_list(PollLength::ALL, Some(p.length), move |l| pm(PollMsg::Length(l)))
                        .text_size(fs * 0.9)
                        .padding(8),
                ]
                .spacing(6),
                space::horizontal(),
                checkbox(p.multiple)
                    .label("Allow more than one answer")
                    .on_toggle(move |on| pm(PollMsg::Multiple(on)))
                    .text_size(fs * 0.9),
            ]
            .align_y(Alignment::End),
            row![
                space::horizontal(),
                button(text("Cancel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
                button(text("Post Poll").size(fs))
                    .padding([8, 16])
                    .on_press_maybe(p.ready().then_some(pm(PollMsg::Send)))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(8),
        ]
        .spacing(10);
        container(body).width(480).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use celerity_proto::PollOption;

    fn poll(votes: &[&[i64]], closed: bool, ends_at: Option<i64>) -> Poll {
        Poll {
            question: "Pizza?".into(),
            options: votes
                .iter()
                .enumerate()
                .map(|(i, v)| PollOption { id: i as u32, text: format!("{i}"), votes: v.to_vec() })
                .collect(),
            multiple: true,
            ends_at,
            closed,
        }
    }

    #[test]
    fn poll_maths() {
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(2, 3), 67);
        assert_eq!(percent(0, 0), 0);
        // People who picked several answers count once in the total.
        let p = poll(&[&[1, 2], &[2], &[]], false, Some(10 * 3_600_000));
        assert_eq!(p.voters(), 2);
        assert_eq!(percent(p.options[0].votes.len(), p.voters()), 100);
        assert_eq!(leaders(&p), vec![0]);
        assert!(leaders(&poll(&[&[], &[]], true, None)).is_empty());
        assert_eq!(leaders(&poll(&[&[1], &[2]], true, None)), vec![0, 1]);
    }

    #[test]
    fn poll_footer() {
        let p = poll(&[&[1, 2], &[3]], false, Some(4 * 3_600_000 + 20 * 60_000));
        assert_eq!(footer(&p, 0), "3 votes · 4h 20m left");
        assert_eq!(footer(&poll(&[&[1]], true, Some(5)), 0), "1 vote · Final results");
        assert_eq!(footer(&poll(&[&[]], false, None), 0), "0 votes");
        assert_eq!(time_left(30_000), "less than a minute");
        assert_eq!(time_left(12 * 60_000), "12m");
        assert_eq!(time_left(3 * 3_600_000), "3h");
        assert_eq!(time_left(2 * 86_400_000 + 5 * 3_600_000), "2d 5h");
        assert_eq!(time_left(7 * 86_400_000), "7d");
        assert_eq!(PollLength::Week.secs(), 7 * 24 * 3600);
    }

    #[test]
    fn drafts_need_a_question_and_two_answers() {
        let mut d = PollDraft::default();
        assert!(!d.ready());
        d.question = "Pizza?".into();
        d.answers = vec!["Yes".into(), "  ".into()];
        assert!(!d.ready());
        d.answers[1] = "No".into();
        assert!(d.ready());
        assert_eq!(d.filled(), vec!["Yes", "No"]);
    }
}
