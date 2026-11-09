//! Group conversations: their row in the direct messages list, the stacked avatars,
//! the "new message" picker and the group settings dialog.

use celerity_proto::{GroupChannel, User, UserId};
use iced::widget::{Id, button, checkbox, column, container, row, scrollable, space, stack, text, text_input};
use iced::{Alignment, Border, Element, Length, Padding};

use crate::app::{App, Message, Session};
use crate::app_groups::{GroupEdit, GroupMsg, GroupPick, MAX_PICK};
use crate::style::{self, bold, card, label, semibold};
use crate::theme::with_alpha;

fn gr(m: GroupMsg) -> Message {
    Message::Groups(m)
}

impl App {
    /// Two members' avatars, one over the other's corner.
    pub(crate) fn group_avatar<'a>(&self, s: &Session, g: &GroupChannel, size: f32) -> Element<'a, Message> {
        let others: Vec<&User> =
            g.members.iter().filter(|id| **id != s.me).filter_map(|id| s.users.get(id)).take(2).collect();
        let small = size * 0.68;
        let cutout = self.theme.sidebar.0;
        match others.as_slice() {
            [a, b] => stack![
                container(self.avatar(a, small)).width(size).height(size),
                container(container(self.avatar(b, small)).padding(2).style(move |_| container::Style {
                    background: Some(cutout.into()),
                    border: Border { radius: (small / 2.0 + 2.0).into(), ..Default::default() },
                    ..Default::default()
                }))
                .width(size)
                .height(size)
                .align_x(Alignment::End)
                .align_y(Alignment::End),
            ]
            .into(),
            [a] => self.avatar(a, size),
            _ => container(crate::icons::icon(&crate::icons::PEOPLE, size * 0.6, self.theme.muted.0))
                .center(size)
                .style(card(self.theme.hover.0, size / 2.0))
                .into(),
        }
    }

    /// A group in the conversations list.
    pub(crate) fn group_row<'a>(&'a self, s: &'a Session, g: &'a GroupChannel) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let selected = s.current == Some(g.id);
        let muted = self.settings.channel_muted(&self.settings.server, g.id);
        let unread = s.is_unread(g.id) && !selected && !muted;
        let mut name = text(crate::view::truncate(&s.group_name(g), 22)).size(fs * 0.95);
        if muted && !selected {
            name = name.color(with_alpha(t.muted.0, 0.6));
        }
        if unread {
            name = name.font(bold()).color(t.header.0);
        }
        let info = column![name, text(format!("{} members", g.members.len())).size(fs * 0.72).color(t.muted.0)];
        let mut line =
            row![self.group_avatar(s, g, 32.0), info.width(Length::Fill)].spacing(10).align_y(Alignment::Center);
        if s.mention_count(g.id) > 0 && !selected {
            line = line.push(crate::view::mention_badge(t, s.mention_count(g.id), fs));
        } else if unread {
            line = line.push(container(space()).width(8).height(8).style(style::flat(t.header.0, 4.0)));
        }
        let row = style::fade_row(
            t,
            button(line).width(Length::Fill).padding([5, 8]).on_press(Message::SelectChannel(g.id)),
            selected,
        );
        let id = g.id;
        crate::widgets::context_menu(row, move || {
            let muted = self.settings.channel_muted(&self.settings.server, id);
            self.simple_menu(vec![
                ("Mark as read".into(), Message::MarkChannelRead(id)),
                (if muted { "Unmute group" } else { "Mute group" }.into(), Message::ToggleMuteChannel(id)),
                ("Group settings".into(), gr(GroupMsg::OpenSettings(id))),
                ("Leave group".into(), gr(GroupMsg::AskLeave(id))),
            ])
        })
        .into()
    }

    /// Pick one person for a direct message, or several for a group.
    pub(crate) fn new_group_dialog<'a>(&'a self, s: &'a Session, p: &'a GroupPick) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let q = p.query.trim().to_lowercase();
        let mut users: Vec<&User> = s
            .users
            .values()
            .filter(|u| u.id != s.me && !u.removed)
            .filter(|u| q.is_empty() || u.name().to_lowercase().contains(&q) || u.username.to_lowercase().contains(&q))
            .collect();
        users.sort_by_key(|u| (!s.online.contains(&u.id), u.name().to_lowercase()));
        let full = p.picked.len() >= MAX_PICK;
        let mut list = column![].spacing(2);
        for u in users {
            let picked = p.picked.contains(&u.id);
            let id = u.id;
            let line = row![
                self.avatar_status(u, 32.0, s.online.contains(&u.id), t.chat.0),
                column![
                    text(u.name().to_owned()).size(fs).font(semibold()),
                    text(u.username.clone()).size(fs * 0.75).color(t.muted.0)
                ]
                .width(Length::Fill),
                checkbox(picked).on_toggle_maybe((picked || !full).then_some(move |_| gr(GroupMsg::TogglePick(id)))),
            ]
            .spacing(12)
            .align_y(Alignment::Center);
            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding([6, 8])
                    .on_press_maybe((picked || !full).then_some(gr(GroupMsg::TogglePick(id))))
                    .style(style::row_button(t, picked)),
            );
        }
        let names: Vec<String> =
            p.picked.iter().filter_map(|id| s.users.get(id)).map(|u| u.name().to_string()).collect();
        let (action, hint) = match p.picked.len() {
            0 => ("Pick someone", "Pick one person for a direct message, or up to nine for a group."),
            1 => ("Message", "Only the two of you can see direct messages."),
            _ => ("Create Group", "Everyone in a group sees its messages. You can add more people later."),
        };
        let body = column![
            text("New message").size(fs * 1.3).font(bold()).color(t.header.0),
            text(hint).size(fs * 0.85).color(t.muted.0),
            text_input("Find people", &p.query)
                .id(Id::new("group-search"))
                .on_input(|v| gr(GroupMsg::PickerQuery(v)))
                .on_submit(gr(GroupMsg::Start))
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
            text(if names.is_empty() {
                String::new()
            } else {
                format!("{} ({} of {MAX_PICK})", names.join(", "), names.len())
            })
            .size(fs * 0.85)
            .color(t.text.0),
            scrollable(list).height(300).style(style::scroll(t)),
            row![
                space::horizontal(),
                button(text("Cancel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
                button(text(action).size(fs))
                    .padding([8, 16])
                    .on_press_maybe((!p.picked.is_empty()).then_some(gr(GroupMsg::Start)))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(8),
        ]
        .spacing(10);
        container(body).width(440).padding(20).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// Rename, add and remove people (the group's owner), or leave.
    pub(crate) fn group_settings_dialog<'a>(&'a self, s: &'a Session, e: &'a GroupEdit) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(g) = s.group(e.id) else { return space().into() };
        let owner = g.owner_id == s.me;
        let mut body = column![text("Group settings").size(fs * 1.3).font(bold()).color(t.header.0)].spacing(12);
        if owner {
            let changed = e.name.trim() != g.name;
            body = body.push(label(t, "Group name", fs)).push(
                row![
                    text_input(&s.group_name(g), &e.name)
                        .on_input(|v| gr(GroupMsg::Name(v)))
                        .on_submit(gr(GroupMsg::SaveName))
                        .padding(10)
                        .size(fs)
                        .style(style::input(t, t.rail.0)),
                    button(text("Save").size(fs * 0.9))
                        .padding([9, 14])
                        .on_press_maybe(changed.then_some(gr(GroupMsg::SaveName)))
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
            body =
                body.push(text("Leave it empty to name the group after its members.").size(fs * 0.8).color(t.muted.0));
        } else {
            body = body.push(text(s.group_name(g)).size(fs * 1.05).font(semibold()).color(t.header.0));
        }

        body = body.push(label(t, format!("Members — {}", g.members.len()), fs));
        let mut members = column![].spacing(2);
        let mut sorted: Vec<&User> = g.members.iter().filter_map(|id| s.users.get(id)).collect();
        sorted.sort_by_key(|u| (u.id != g.owner_id, u.name().to_lowercase()));
        for u in sorted {
            let mut line = row![self.avatar(u, 28.0), text(u.name().to_owned()).size(fs * 0.95).color(t.header.0)]
                .spacing(10)
                .align_y(Alignment::Center);
            if u.id == g.owner_id {
                line = line.push(
                    container(text("Owner").size(fs * 0.7).font(bold()).color(crate::theme::contrast_text(t.accent.0)))
                        .padding([1, 6])
                        .style(card(t.accent.0, 8.0)),
                );
            }
            line = line.push(space::horizontal());
            if owner && u.id != s.me {
                let id = u.id;
                line = line.push(
                    button(text("Remove").size(fs * 0.8))
                        .padding([3, 10])
                        .on_press(gr(GroupMsg::Remove(id)))
                        .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                );
            }
            members = members.push(container(line).padding([4, 6]));
        }
        body = body.push(container(scrollable(members).height(Length::Shrink).style(style::scroll(t))).max_height(220));

        if owner && g.members.len() < celerity_proto::social::MAX_GROUP_SIZE {
            body = body.push(label(t, "Add people", fs)).push(
                text_input("Find someone to add", &e.query)
                    .on_input(|v| gr(GroupMsg::Query(v)))
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.rail.0)),
            );
            let q = e.query.trim().to_lowercase();
            if !q.is_empty() {
                let found: Vec<&User> = s
                    .users
                    .values()
                    .filter(|u| !g.members.contains(&u.id) && !u.removed)
                    .filter(|u| u.name().to_lowercase().contains(&q) || u.username.to_lowercase().contains(&q))
                    .take(5)
                    .collect();
                let mut col = column![].spacing(2);
                if found.is_empty() {
                    col = col.push(text("Nobody by that name.").size(fs * 0.85).color(t.muted.0));
                }
                for u in found {
                    let id: UserId = u.id;
                    col = col.push(
                        button(
                            row![
                                self.avatar(u, 24.0),
                                text(u.name().to_owned()).size(fs * 0.9),
                                space::horizontal(),
                                text("Add").size(fs * 0.8).color(t.accent.0)
                            ]
                            .spacing(10)
                            .align_y(Alignment::Center),
                        )
                        .width(Length::Fill)
                        .padding([4, 8])
                        .on_press(gr(GroupMsg::Add(id)))
                        .style(style::row_button(t, false)),
                    );
                }
                body = body.push(col);
            }
        }
        let danger = t.danger.0;
        body = body.push(
            row![
                button(text("Leave Group").size(fs * 0.9))
                    .padding([8, 12])
                    .on_press(gr(GroupMsg::AskLeave(g.id)))
                    .style(style::link_button(danger, style::darken(danger, 0.2))),
                space::horizontal(),
                button(text("Done").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .align_y(Alignment::Center),
        );
        container(body).width(440).padding(Padding::from(22)).style(card(t.chat.0, t.radius * 1.5)).into()
    }
}
