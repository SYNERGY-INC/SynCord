//! The thread panel (a thread's messages and its own message box), the list of a channel's
//! threads, and the "N replies" bar under a message that started a thread.

use celerity_proto::{Message as ChatMessage, Thread, User};
use iced::widget::{Id, button, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Border, Element, Length, Padding};

use crate::app::{App, Message, Session};
use crate::app_expressions::plain_emoji;
use crate::app_threads::{THREAD_COMPOSER_ID, THREAD_MESSAGES_ID, ThreadList, ThreadMsg};
use crate::icons;
use crate::style::{self, bold, card, label, semibold};
use crate::theme::with_alpha;

fn th(m: ThreadMsg) -> Message {
    Message::Threads(m)
}

/// "1 reply", "12 replies".
pub fn replies(n: u32) -> String {
    if n == 1 { "1 reply".into() } else { format!("{n} replies") }
}

impl App {
    /// Under a message that started a thread: its name and how many replies, to open it.
    pub(crate) fn thread_bar<'a>(&'a self, s: &'a Session, m: &'a ChatMessage) -> Option<Element<'a, Message>> {
        let summary = m.thread.as_ref()?;
        let t = &self.theme;
        let fs = self.fs();
        let thread = s.threads.get(&summary.id);
        let count = thread.map_or(summary.message_count, |x| x.message_count);
        let archived = thread.map_or(summary.archived, |x| x.archived);
        let activity = thread.map_or(summary.last_activity, |x| x.last_activity);
        let mut line = row![
            icons::icon(&icons::THREAD, fs * 1.05, t.accent.0),
            text(replies(count)).size(fs * 0.85).font(semibold()).color(t.accent.0),
            text("·").size(fs * 0.85).color(t.muted.0),
            text(crate::view::truncate(&plain_emoji(&summary.name), 40))
                .size(fs * 0.85)
                .color(t.header.0)
                .wrapping(iced::widget::text::Wrapping::None),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let when = if archived {
            "Archived".to_string()
        } else if activity > 0 {
            crate::view::long_time(activity).trim_start_matches("Today at ").to_string()
        } else {
            String::new()
        };
        if !when.is_empty() {
            line = line.push(text(when).size(fs * 0.75).color(t.muted.0).wrapping(iced::widget::text::Wrapping::None));
        }
        let (floating, hover, edge) = (t.floating.0, style::darken(t.floating.0, 0.12), t.hover.0);
        let open = s.open_thread == Some(summary.id);
        let accent = t.accent.0;
        Some(
            button(line)
                .padding([5, 10])
                .on_press(th(ThreadMsg::Open(summary.id)))
                .style(move |_, status| button::Style {
                    background: Some(
                        match status {
                            button::Status::Hovered | button::Status::Pressed => hover,
                            _ => floating,
                        }
                        .into(),
                    ),
                    border: Border { radius: 8.0.into(), width: 1.0, color: if open { accent } else { edge } },
                    ..Default::default()
                })
                .into(),
        )
    }

    /// The open thread, in the side panel.
    pub(crate) fn thread_panel<'a>(&'a self, s: &'a Session, thread: &'a Thread) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let parent = s.channels.iter().find(|c| c.id == thread.parent_id).map(|c| c.name.as_str()).unwrap_or("?");
        let tool = |glyph: &'static std::sync::LazyLock<iced::widget::svg::Handle>, tip: &'static str, msg: Message| {
            iced::widget::tooltip(
                button(icons::icon(glyph, fs, t.muted.0)).padding(4).on_press(msg).style(style::icon_button(t)),
                container(text(tip).size(fs * 0.8)).padding([4, 8]).style(card(t.floating.0, 6.0)),
                iced::widget::tooltip::Position::Bottom,
            )
        };
        let title: Element<'a, Message> = match &self.thread_ui.renaming {
            Some(name) => row![
                text_input("Thread name", name)
                    .id(Id::new("thread-rename"))
                    .on_input(|v| th(ThreadMsg::RenameInput(v)))
                    .on_submit(th(ThreadMsg::SaveRename))
                    .padding(6)
                    .size(fs * 0.9)
                    .style(style::input(t, t.rail.0)),
                button(text("Cancel").size(fs * 0.75))
                    .padding([2, 6])
                    .on_press(th(ThreadMsg::CancelRename))
                    .style(style::link_button(t.muted.0, t.header.0)),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
            None => column![
                text(plain_emoji(&thread.name)).size(fs * 0.95).font(bold()).color(t.header.0),
                text(format!("Thread in #{parent}")).size(fs * 0.75).color(t.muted.0),
            ]
            .spacing(1)
            .into(),
        };
        let mut header = row![icons::icon(&icons::THREAD, fs * 1.2, t.muted.0), container(title).width(Length::Fill)]
            .spacing(8)
            .align_y(Alignment::Center);
        if s.can_edit_thread(thread) && self.thread_ui.renaming.is_none() {
            header = header.push(
                button(text("Rename").size(fs * 0.75))
                    .padding([2, 6])
                    .on_press(th(ThreadMsg::StartRename))
                    .style(style::icon_button(t)),
            );
            let (label_text, archived) = if thread.archived { ("Unarchive", false) } else { ("Archive", true) };
            header = header.push(
                button(text(label_text).size(fs * 0.75))
                    .padding([2, 6])
                    .on_press(th(ThreadMsg::Archive(thread.id, archived)))
                    .style(style::icon_button(t)),
            );
        }
        if s.can_delete_thread() {
            header = header.push(tool(&icons::TRASH, "Delete thread", th(ThreadMsg::AskDelete(thread.id))));
        }
        header = header.push(tool(&icons::CLOSE, "Close", th(ThreadMsg::Close)));

        let state = s.chans.get(&thread.id);
        let messages: &[ChatMessage] = state.map(|st| st.messages.as_slice()).unwrap_or(&[]);
        let mut list = column![].padding(Padding { top: 8.0, bottom: 8.0, left: 4.0, right: 4.0 });
        match state {
            Some(st) if st.loaded && st.reached_start => {
                // Where it started.
                let starter = thread.parent_message_id.and_then(|id| {
                    s.chans.get(&thread.parent_id).and_then(|st| st.messages.iter().find(|m| m.id == id))
                });
                let who = s.users.get(&thread.owner_id).map(User::name).unwrap_or("Someone");
                let mut start = column![
                    container(icons::icon(&icons::THREAD, 28.0, t.accent.0))
                        .center(52)
                        .style(card(with_alpha(t.accent.0, 0.16), 16.0)),
                    text(plain_emoji(&thread.name)).size(fs * 1.3).font(bold()).color(t.header.0),
                    text(format!("Started by {who}")).size(fs * 0.85).color(t.muted.0),
                ]
                .spacing(6);
                if let Some(m) = starter {
                    start = start.push(self.message_card(s, m, None));
                }
                list = list.push(container(start).padding([8, 12]));
            }
            Some(st) if st.loaded => {
                let label = if st.loading { "Loading..." } else { "Load older messages" };
                list = list.push(
                    container(
                        button(text(label).size(fs * 0.85))
                            .padding([4, 10])
                            .on_press_maybe((!st.loading).then_some(th(ThreadMsg::LoadOlder)))
                            .style(style::icon_button(t)),
                    )
                    .center_x(Length::Fill)
                    .padding(8),
                );
            }
            _ => list = list.push(container(text("Loading...").color(t.muted.0)).center_x(Length::Fill).padding(24)),
        }
        let mut prev: Option<&ChatMessage> = None;
        for m in messages {
            let grouped = m.reply.is_none()
                && prev.is_some_and(|p| {
                    p.author_id == m.author_id && p.kind.is_normal() && m.created_at - p.created_at < 7 * 60 * 1000
                });
            if !grouped && prev.is_some() {
                list = list.push(space().height(10));
            }
            list = list.push(self.message_row(s, m, grouped));
            prev = Some(m);
        }
        let list = scrollable(list)
            .id(Id::new(THREAD_MESSAGES_ID))
            .anchor_bottom()
            .height(Length::Fill)
            .width(Length::Fill)
            .style(style::scroll(t));

        let mut bottom = column![].spacing(6).padding(Padding { left: 10.0, right: 10.0, top: 0.0, bottom: 4.0 });
        if thread.archived {
            bottom = bottom.push(
                text("This thread is archived. Sending a message brings it back.").size(fs * 0.8).color(t.muted.0),
            );
        }
        if let Some(reply) = self.thread_ui.replying.and_then(|id| messages.iter().find(|m| m.id == id)) {
            let name = s.users.get(&reply.author_id).map(User::name).unwrap_or("someone");
            bottom = bottom.push(
                row![
                    text(format!("Replying to {name}")).size(fs * 0.8).color(t.muted.0).width(Length::Fill),
                    button(icons::icon(&icons::CLOSE, fs * 0.8, t.muted.0))
                        .padding(2)
                        .on_press(th(ThreadMsg::CancelReply))
                        .style(style::icon_button(t)),
                ]
                .align_y(Alignment::Center),
            );
        }
        let (floating, edge, radius) = (t.floating.0, with_alpha(t.header.0, 0.07), t.radius);
        bottom = bottom.push(
            container(
                text_input(
                    &format!("Message {}", crate::view::truncate(&plain_emoji(&thread.name), 30)),
                    &self.thread_ui.draft,
                )
                .id(Id::new(THREAD_COMPOSER_ID))
                .on_input(|v| th(ThreadMsg::Draft(v)))
                .on_submit(th(ThreadMsg::Send))
                .padding(12)
                .size(fs)
                .style(style::pill_input(t, floating)),
            )
            .style(move |_| container::Style {
                border: Border { radius: (radius * 2.6).clamp(10.0, 24.0).into(), width: 1.0, color: edge },
                ..Default::default()
            }),
        );
        let typers: Vec<&str> = s
            .typing
            .get(&thread.id)
            .map(|m| m.keys().filter_map(|id| s.users.get(id).map(User::name)).collect())
            .unwrap_or_default();
        let typing = match typers.as_slice() {
            [] => String::new(),
            [a] => format!("{a} is typing..."),
            _ => "Several people are typing...".into(),
        };
        bottom = bottom.push(text(typing).size(fs * 0.75).font(semibold()).color(t.text.0).height(fs * 1.2));

        container(
            column![container(header).padding([12, 12]), style::divider(with_alpha(t.header.0, 0.06)), list, bottom]
                .spacing(4),
        )
        .width(380)
        .height(Length::Fill)
        .into()
    }

    /// A channel's threads: active ones, then archived ones.
    pub(crate) fn thread_list_panel<'a>(&'a self, s: &'a Session, list: &'a ThreadList) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let parent = s.channels.iter().find(|c| c.id == list.channel).map(|c| c.name.as_str()).unwrap_or("?");
        let header = row![
            column![
                text("Threads").size(fs * 0.95).font(bold()).color(t.header.0),
                text(format!("in #{parent}")).size(fs * 0.75).color(t.muted.0),
            ]
            .width(Length::Fill),
            button(icons::icon(&icons::CLOSE, fs, t.muted.0))
                .padding(4)
                .on_press(th(ThreadMsg::ToggleList))
                .style(style::icon_button(t)),
        ]
        .align_y(Alignment::Center);
        // Until the server answers, the active threads we already know of.
        let known: Vec<&Thread> = match &list.threads {
            Some(all) => all.iter().collect(),
            None => {
                let mut v: Vec<&Thread> =
                    s.threads.values().filter(|x| x.parent_id == list.channel && !x.archived).collect();
                v.sort_by_key(|x| std::cmp::Reverse(x.last_activity));
                v
            }
        };
        let (active, archived): (Vec<&Thread>, Vec<&Thread>) = known.into_iter().partition(|x| !x.archived);
        let mut col = column![].spacing(6).padding(Padding { top: 0.0, bottom: 12.0, left: 12.0, right: 12.0 });
        if active.is_empty() && archived.is_empty() {
            col = col.push(
                text(if list.threads.is_none() {
                    "Loading..."
                } else {
                    "No threads here yet. Point at a message and click the thread button to start one."
                })
                .size(fs * 0.85)
                .color(t.muted.0),
            );
        }
        for (title, group) in [("Active", active), ("Archived", archived)] {
            if group.is_empty() {
                continue;
            }
            col = col.push(label(t, format!("{title} — {}", group.len()), fs));
            for x in group {
                let who = s.users.get(&x.owner_id).map(User::name).unwrap_or("someone");
                let when = if x.last_activity > 0 { crate::view::long_time(x.last_activity) } else { String::new() };
                let body = column![
                    text(plain_emoji(&x.name)).size(fs * 0.95).font(semibold()).color(t.header.0),
                    text(format!("{} · started by {who} · {when}", replies(x.message_count)))
                        .size(fs * 0.75)
                        .color(t.muted.0),
                ]
                .spacing(2);
                col = col.push(
                    button(row![icons::icon(&icons::THREAD, fs * 1.2, t.muted.0), body].spacing(10))
                        .width(Length::Fill)
                        .padding([8, 10])
                        .on_press(th(ThreadMsg::Open(x.id)))
                        .style(style::row_button(t, false)),
                );
            }
        }
        container(
            column![container(header).padding([12, 12]), scrollable(col).height(Length::Fill).style(style::scroll(t)),]
                .spacing(4),
        )
        .width(340)
        .height(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reply_counts() {
        assert_eq!(super::replies(1), "1 reply");
        assert_eq!(super::replies(0), "0 replies");
        assert_eq!(super::replies(12), "12 replies");
    }
}
