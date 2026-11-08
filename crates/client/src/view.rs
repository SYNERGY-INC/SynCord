//! All screens. Layout follows the familiar four-column chat layout:
//! server bar | channel list | messages | member list.

use chrono::{Local, TimeZone};
use iced::widget::{
    Id, button, center, column, container, mouse_area, opaque, row, scrollable, slider, space, stack, text,
    text_input, toggler,
};
use iced::{Alignment, Border, Color, Element, Length, Padding};
use syncord_proto::{AvatarRing, Channel, Message as ChatMessage, User};

use crate::app::{App, COMPOSER_ID, DraftField, MESSAGES_ID, Message, Overlay, Session, SettingsTab};
use crate::style::{self, BOLD, SEMIBOLD, avatar, avatar_with_status, bg, card, divider, label};
use crate::theme::{AppTheme, contrast_text, rgb, themes_dir, with_alpha};

/// Messages from the same person within this window are grouped under one header.
const GROUP_WINDOW_MS: i64 = 7 * 60 * 1000;

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        let Some(session) = &self.session else { return self.login_view() };

        let base: Element<'_, Message> = match self.settings_tab {
            Some(tab) => self.settings_view(session, tab),
            None => self.main_view(session),
        };

        match &self.overlay {
            None => base,
            Some(overlay) => {
                let content = match overlay {
                    Overlay::Profile(id) => match session.users.get(id) {
                        Some(u) => style::profile_card(
                            &self.theme,
                            &u.username,
                            &u.profile,
                            u.is_owner,
                            session.online.contains(id),
                            self.fs(),
                        ),
                        None => return base,
                    },
                    Overlay::NewChannel { name, topic } => self.new_channel_dialog(name, topic),
                    Overlay::ConfirmDelete(id) => self.confirm_delete_dialog(session, *id),
                };
                let backdrop = mouse_area(
                    center(opaque(content))
                        .style(bg(Color::from_rgba(0.0, 0.0, 0.0, 0.6)))
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .on_press(Message::CloseOverlay);
                stack![base, opaque(backdrop)].into()
            }
        }
    }

    // ------------------------------------------------------------------ login

    fn login_view(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let f = &self.login;

        let field = |name: &'static str, value: &str, placeholder: &'static str, on: fn(String) -> Message| {
            column![
                label(t, name, fs),
                text_input(placeholder, value)
                    .on_input(on)
                    .on_submit(Message::SubmitLogin)
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.rail.0)),
            ]
            .spacing(6)
        };

        let (title, subtitle, action, switch) = if f.register {
            ("Create an account", "Ask whoever runs the server for the invite code.", "Continue", "Already have an account? Log in")
        } else {
            ("Welcome back!", "We're so excited to see you again!", "Log In", "Need an account? Register")
        };

        let mut form = column![
            column![
                text(title).size(fs * 1.6).font(BOLD).color(t.header.0),
                text(subtitle).size(fs).color(t.muted.0),
            ]
            .spacing(6)
            .align_x(Alignment::Center)
            .width(Length::Fill),
            field("Server address", &f.server, "e.g. 100.64.0.5 or chat.example.com", Message::LoginServer),
            field("Username", &f.username, "", Message::LoginUsername),
            column![
                label(t, "Password", fs),
                text_input("", &f.password)
                    .secure(true)
                    .on_input(Message::LoginPassword)
                    .on_submit(Message::SubmitLogin)
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.rail.0)),
            ]
            .spacing(6),
        ]
        .spacing(18);

        if f.register {
            form = form.push(field("Invite code", &f.invite, "", Message::LoginInvite));
        }
        if let Some(err) = &f.error {
            form = form.push(text(err.clone()).size(fs * 0.9).color(t.danger.0));
        }

        let label_text = if f.busy { "Connecting..." } else { action };
        form = form.push(
            button(center(text(label_text).size(fs).font(SEMIBOLD)).height(fs * 2.6))
                .width(Length::Fill)
                .padding(0)
                .on_press_maybe((!f.busy).then_some(Message::SubmitLogin))
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        );
        form = form.push(
            button(text(switch).size(fs * 0.9))
                .padding(0)
                .on_press(Message::ToggleRegister)
                .style(style::link_button(t.accent.0, t.header.0)),
        );

        center(container(form).width(460).padding(32).style(card(t.sidebar.0, t.radius * 1.5)))
            .style(bg(t.rail.0))
            .into()
    }

    // ------------------------------------------------------------------ main

    fn main_view<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let mut layout = row![self.server_rail(s), self.channel_sidebar(s), self.chat_area(s)];
        if self.settings.show_members {
            layout = layout.push(self.member_list(s));
        }
        layout.height(Length::Fill).into()
    }

    fn server_rail<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let initials: String =
            s.community.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect::<String>().to_uppercase();
        let accent = t.accent.0;
        let icon = container(text(initials).size(self.fs() * 1.1).font(BOLD).color(contrast_text(accent)))
            .center(48)
            .style(move |_| container::Style {
                background: Some(accent.into()),
                border: Border { radius: 16.0.into(), ..Default::default() },
                ..Default::default()
            });
        // White "pill" on the left edge marks the selected server.
        let header = t.header.0;
        let pill = container(space()).width(4).height(40).style(move |_| container::Style {
            background: Some(header.into()),
            border: Border { radius: iced::border::right(4.0), ..Default::default() },
            ..Default::default()
        });
        let entry = row![pill, space().width(8), icon].align_y(Alignment::Center);

        container(column![entry, container(divider(t.hover.0)).width(32).padding([0, 0])].spacing(10).align_x(Alignment::Center))
            .padding([12, 0])
            .width(72)
            .height(Length::Fill)
            .style(bg(t.rail.0))
            .into()
    }

    fn channel_sidebar<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();

        let header = container(text(s.community.clone()).size(fs * 1.05).font(BOLD).color(t.header.0))
            .padding([0, 16])
            .height(48)
            .width(Length::Fill)
            .align_y(Alignment::Center);

        let mut category = row![label(t, "Text channels", fs), space::horizontal()].align_y(Alignment::Center);
        if s.is_owner() {
            category = category.push(
                button(text("+").size(fs * 1.2))
                    .padding([0, 6])
                    .on_press(Message::OpenNewChannel)
                    .style(style::link_button(t.muted.0, t.header.0)),
            );
        }

        let mut list = column![container(category).padding(Padding { left: 8.0, top: 16.0, right: 4.0, bottom: 4.0 })]
            .spacing(2);
        for ch in &s.channels {
            let selected = s.current == Some(ch.id);
            let unread = s.unread.contains(&ch.id);
            let name_color = if unread && !selected { Some(t.header.0) } else { None };
            let mut name = text(ch.name.clone()).size(fs);
            if unread {
                name = name.font(SEMIBOLD);
            }
            if let Some(c) = name_color {
                name = name.color(c);
            }
            let line = row![text("#").size(fs * 1.25).color(t.muted.0), name].spacing(6).align_y(Alignment::Center);
            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding([6, 8])
                    .on_press(Message::SelectChannel(ch.id))
                    .style(style::row_button(t, selected)),
            );
        }

        container(column![
            header,
            divider(style::darken(t.sidebar.0, 0.25)),
            scrollable(container(list).padding([0, 8])).height(Length::Fill).style(style::scroll(t)),
            self.user_panel(s),
        ])
        .width(240)
        .height(Length::Fill)
        .style(bg(t.sidebar.0))
        .into()
    }

    fn user_panel<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(me) = s.me() else { return space().into() };
        let status = if !self.reconnecting {
            if me.profile.status_text.is_empty() { "Online".to_string() } else { me.profile.status_text.clone() }
        } else {
            "Reconnecting...".to_string()
        };
        let content = row![
            avatar_with_status(t, me.name(), &me.profile, 32.0, !self.reconnecting, t.user_panel.0),
            column![
                text(me.name().to_owned()).size(fs * 0.9).font(SEMIBOLD).color(t.header.0),
                text(status).size(fs * 0.75).color(t.muted.0),
            ]
            .spacing(1)
            .width(Length::Fill),
            button(text("Settings").size(fs * 0.8))
                .padding([4, 8])
                .on_press(Message::OpenSettings)
                .style(style::icon_button(t)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        container(content).padding([10, 8]).width(Length::Fill).style(bg(t.user_panel.0)).into()
    }

    fn chat_area<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let channel = s.current_channel();

        // Header: "# name | topic" ............ [Members]
        let mut header = row![].spacing(10).align_y(Alignment::Center);
        if let Some(ch) = channel {
            header = header
                .push(text("#").size(fs * 1.5).color(t.muted.0))
                .push(text(ch.name.clone()).size(fs * 1.05).font(BOLD).color(t.header.0));
            if !ch.topic.is_empty() {
                header = header
                    .push(container(space()).width(1).height(24).style(bg(t.hover.0)))
                    .push(text(ch.topic.clone()).size(fs * 0.9).color(t.muted.0));
            }
        }
        header = header.push(space::horizontal());
        if let Some(ch) = channel.filter(|_| s.is_owner() && s.channels.len() > 1) {
            header = header.push(
                button(text("Delete channel").size(fs * 0.8))
                    .padding([4, 8])
                    .on_press(Message::AskDeleteChannel(ch.id))
                    .style(style::icon_button(t)),
            );
        }
        header = header.push(
            button(text(if self.settings.show_members { "Hide members" } else { "Members" }).size(fs * 0.8))
                .padding([4, 8])
                .on_press(Message::ToggleMembers)
                .style(style::icon_button(t)),
        );
        let header = container(header).padding([0, 16]).height(48).width(Length::Fill).align_y(Alignment::Center);

        let mut col = column![header, divider(style::darken(t.chat.0, 0.25))];

        if self.reconnecting {
            col = col.push(
                container(text("Connection lost. Reconnecting...").size(fs * 0.85).color(Color::WHITE))
                    .padding([6, 16])
                    .width(Length::Fill)
                    .style(bg(t.danger.0)),
            );
        }
        if let Some(notice) = &self.notice {
            col = col.push(
                container(
                    row![
                        text(notice.clone()).size(fs * 0.85).color(contrast_text(t.floating.0)).width(Length::Fill),
                        button(text("Dismiss").size(fs * 0.8))
                            .padding([2, 8])
                            .on_press(Message::DismissNotice)
                            .style(style::icon_button(t)),
                    ]
                    .align_y(Alignment::Center),
                )
                .padding([6, 16])
                .width(Length::Fill)
                .style(bg(t.floating.0)),
            );
        }

        col = col.push(self.message_list(s, channel));
        if let Some(ch) = channel {
            col = col.push(self.composer(s, ch));
        }
        container(col).width(Length::Fill).height(Length::Fill).style(bg(t.chat.0)).into()
    }

    fn message_list<'a>(&'a self, s: &'a Session, channel: Option<&'a Channel>) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(channel) = channel else {
            return center(text("No channels yet.").color(t.muted.0)).into();
        };
        let state = s.chans.get(&channel.id);
        let messages: &[ChatMessage] = state.map(|st| st.messages.as_slice()).unwrap_or(&[]);

        let mut list = column![].padding(Padding { top: 16.0, bottom: 16.0, left: 0.0, right: 0.0 });

        match state {
            Some(st) if st.loaded && st.reached_start => {
                list = list.push(
                    container(
                        column![
                            container(text("#").size(fs * 2.4).color(t.header.0)).center(68).style(card(t.floating.0, 34.0)),
                            text(format!("Welcome to #{}!", channel.name)).size(fs * 2.0).font(BOLD).color(t.header.0),
                            text(format!("This is the start of the #{} channel.", channel.name)).size(fs).color(t.muted.0),
                        ]
                        .spacing(8),
                    )
                    .padding(Padding { left: 16.0, bottom: 16.0, top: 0.0, right: 16.0 }),
                );
            }
            Some(st) if st.loaded => {
                let label = if st.loading { "Loading..." } else { "Load older messages" };
                list = list.push(
                    container(
                        button(text(label).size(fs * 0.85))
                            .padding([4, 10])
                            .on_press_maybe((!st.loading).then_some(Message::LoadOlder))
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
            let grouped = !self.settings.compact
                && prev.is_some_and(|p| p.author_id == m.author_id && m.created_at - p.created_at < GROUP_WINDOW_MS);
            if !grouped && prev.is_some() && !self.settings.compact {
                list = list.push(space().height(14));
            }
            list = list.push(self.message_row(s, m, grouped));
            prev = Some(m);
        }

        scrollable(list)
            .id(Id::new(MESSAGES_ID))
            .anchor_bottom()
            .height(Length::Fill)
            .width(Length::Fill)
            .style(style::scroll(t))
            .into()
    }

    fn message_row<'a>(&'a self, s: &'a Session, m: &'a ChatMessage, grouped: bool) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let author = s.users.get(&m.author_id);
        let name = author.map(User::name).unwrap_or("Unknown user");
        let name_color = author.map(|u| rgb(u.profile.accent_color)).unwrap_or(t.header.0);
        let hovered = self.hovered == Some(m.id);
        let editing = self.editing.as_ref().filter(|(id, _)| *id == m.id);

        let mut body_text = text(m.content.clone()).size(fs).color(t.text.0);
        if m.content.len() < 80 {
            body_text = body_text.width(Length::Shrink);
        }
        let body: Element<'a, Message> = match editing {
            Some((_, value)) => column![
                text_input("", value)
                    .id(Id::new("edit"))
                    .on_input(Message::EditChanged)
                    .on_submit(Message::SubmitEdit)
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.floating.0)),
                row![
                    text("Enter to save (empty deletes)").size(fs * 0.75).color(t.muted.0),
                    button(text("Cancel").size(fs * 0.75))
                        .padding(0)
                        .on_press(Message::CancelEdit)
                        .style(style::link_button(t.accent.0, t.header.0)),
                ]
                .spacing(8),
            ]
            .spacing(4)
            .into(),
            None => {
                let mut r = row![body_text].spacing(6).align_y(Alignment::End);
                if m.edited_at.is_some() {
                    r = r.push(text("(edited)").size(fs * 0.7).color(t.muted.0));
                }
                r.wrap().into()
            }
        };

        let content: Element<'a, Message> = if self.settings.compact {
            row![
                text(short_time(m.created_at)).size(fs * 0.75).color(t.muted.0).width(64),
                button(text(name.to_owned()).size(fs).font(SEMIBOLD).color(name_color))
                    .padding(0)
                    .on_press(Message::ShowProfile(m.author_id))
                    .style(style::link_button(name_color, name_color)),
                container(body).width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Start)
            .into()
        } else if grouped {
            row![
                container(if hovered { text(short_time(m.created_at)).size(fs * 0.68).color(t.muted.0) } else { text("") })
                    .width(40)
                    .align_x(Alignment::End)
                    .padding(Padding { top: 3.0, ..Padding::ZERO }),
                container(body).width(Length::Fill),
            ]
            .spacing(16)
            .into()
        } else {
            let pic: Element<'a, Message> = match author {
                Some(u) => button(avatar(u.name(), &u.profile, 40.0))
                    .padding(0)
                    .on_press(Message::ShowProfile(u.id))
                    .style(style::link_button(t.text.0, t.text.0))
                    .into(),
                None => container(space()).width(40).height(40).style(card(t.hover.0, 20.0)).into(),
            };
            row![
                pic,
                column![
                    row![
                        button(text(name.to_owned()).size(fs).font(SEMIBOLD).color(name_color))
                            .padding(0)
                            .on_press(Message::ShowProfile(m.author_id))
                            .style(style::link_button(name_color, name_color)),
                        text(long_time(m.created_at)).size(fs * 0.75).color(t.muted.0),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                    body,
                ]
                .spacing(2)
                .width(Length::Fill),
            ]
            .spacing(16)
            .into()
        };

        // Hover toolbar for your own messages (the owner can also delete anything).
        let mine = Some(m.author_id) == Some(s.me);
        let mut line = row![container(content).width(Length::Fill)].align_y(Alignment::Start);
        if hovered && editing.is_none() && (mine || s.is_owner()) {
            let mut tools = row![].spacing(2);
            if mine {
                tools = tools.push(
                    button(text("Edit").size(fs * 0.8)).padding([2, 8]).on_press(Message::StartEdit(m.id)).style(style::icon_button(t)),
                );
            }
            let danger = t.danger.0;
            tools = tools.push(
                button(text("Delete").size(fs * 0.8))
                    .padding([2, 8])
                    .on_press(Message::DeleteMessage(m.id))
                    .style(style::link_button(danger, style::darken(danger, 0.2))),
            );
            line = line.push(container(tools).padding(2).style(card(t.floating.0, t.radius * 0.6)));
        }

        let hover_bg = if hovered { Some(with_alpha(style::darken(t.chat.0, 0.12), 1.0)) } else { None };
        mouse_area(container(line).padding([2, 16]).width(Length::Fill).style(move |_| container::Style {
            background: hover_bg.map(Into::into),
            ..Default::default()
        }))
        .on_enter(Message::HoverMessage(Some(m.id)))
        .on_exit(Message::HoverMessage(None))
        .into()
    }

    fn composer<'a>(&'a self, s: &'a Session, ch: &'a Channel) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let input = text_input(&format!("Message #{}", ch.name), &self.composer)
            .id(Id::new(COMPOSER_ID))
            .on_input(Message::ComposerChanged)
            .on_submit(Message::SendMessage)
            .padding(14)
            .size(fs)
            .style(style::input(t, t.floating.0));

        let typers: Vec<&str> = s
            .typing
            .get(&ch.id)
            .map(|m| m.keys().filter_map(|id| s.users.get(id).map(User::name)).collect())
            .unwrap_or_default();
        let typing_text = match typers.as_slice() {
            [] => String::new(),
            [a] => format!("{a} is typing..."),
            [a, b] => format!("{a} and {b} are typing..."),
            _ => "Several people are typing...".into(),
        };

        column![input, text(typing_text).size(fs * 0.75).font(SEMIBOLD).color(t.text.0).height(fs * 1.2)]
            .spacing(2)
            .padding(Padding { left: 16.0, right: 16.0, top: 0.0, bottom: 4.0 })
            .into()
    }

    fn member_list<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut users: Vec<&User> = s.users.values().collect();
        users.sort_by_key(|u| u.name().to_lowercase());
        let (online, offline): (Vec<&User>, Vec<&User>) = users.into_iter().partition(|u| s.online.contains(&u.id));

        let mut list = column![].spacing(2).padding([16, 8]);
        for (title, group, is_online) in [("Online", online, true), ("Offline", offline, false)] {
            if group.is_empty() {
                continue;
            }
            list = list.push(container(label(t, format!("{title} — {}", group.len()), fs)).padding([8, 8]));
            for u in group {
                let name_color = if is_online { rgb(u.profile.accent_color) } else { t.muted.0 };
                let mut info = column![text(u.name().to_owned()).size(fs * 0.95).font(SEMIBOLD).color(name_color)];
                if !u.profile.status_text.is_empty() {
                    info = info.push(text(u.profile.status_text.clone()).size(fs * 0.75).color(t.muted.0));
                }
                let row_content = row![avatar_with_status(t, u.name(), &u.profile, 32.0, is_online, t.sidebar.0), info]
                    .spacing(12)
                    .align_y(Alignment::Center);
                let mut entry = button(row_content)
                    .width(Length::Fill)
                    .padding([4, 8])
                    .on_press(Message::ShowProfile(u.id))
                    .style(style::row_button(t, false));
                if !is_online {
                    entry = entry.style(dimmed(style::row_button(t, false)));
                }
                list = list.push(entry);
            }
        }

        container(scrollable(list).height(Length::Fill).style(style::scroll(t)))
            .width(240)
            .height(Length::Fill)
            .style(bg(t.sidebar.0))
            .into()
    }

    // ------------------------------------------------------------------ dialogs

    fn new_channel_dialog<'a>(&'a self, name: &'a str, topic: &'a str) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let preview = syncord_proto::normalize_channel_name(name);
        let body = column![
            text("Create channel").size(fs * 1.3).font(BOLD).color(t.header.0),
            label(t, "Channel name", fs),
            text_input("new-channel", name)
                .id(Id::new("new-channel"))
                .on_input(Message::NewChannelName)
                .on_submit(Message::CreateChannel)
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
            text(if preview.is_empty() { String::new() } else { format!("Will appear as #{preview}") })
                .size(fs * 0.8)
                .color(t.muted.0),
            label(t, "Topic (optional)", fs),
            text_input("What's this channel about?", topic)
                .on_input(Message::NewChannelTopic)
                .on_submit(Message::CreateChannel)
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
            row![
                space::horizontal(),
                button(text("Cancel").size(fs)).padding([8, 16]).on_press(Message::CloseOverlay).style(style::link_button(t.text.0, t.header.0)),
                button(text("Create Channel").size(fs))
                    .padding([8, 16])
                    .on_press_maybe((!preview.is_empty()).then_some(Message::CreateChannel))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(8),
        ]
        .spacing(10);
        container(body).width(440).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    fn confirm_delete_dialog<'a>(&'a self, s: &'a Session, id: i64) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let name = s.channels.iter().find(|c| c.id == id).map(|c| c.name.as_str()).unwrap_or("?");
        let body = column![
            text("Delete channel").size(fs * 1.3).font(BOLD).color(t.header.0),
            text(format!("Are you sure you want to delete #{name}? All of its messages will be gone for good."))
                .size(fs)
                .color(t.text.0),
            row![
                space::horizontal(),
                button(text("Cancel").size(fs)).padding([8, 16]).on_press(Message::CloseOverlay).style(style::link_button(t.text.0, t.header.0)),
                button(text("Delete Channel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::ConfirmDeleteChannel(id))
                    .style(style::solid_button(t.danger.0, t.radius * 0.6)),
            ]
            .spacing(8),
        ]
        .spacing(14);
        container(body).width(440).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    // ------------------------------------------------------------------ settings

    fn settings_view<'a>(&'a self, s: &'a Session, tab: SettingsTab) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();

        let nav_item = |name: &'static str, target: SettingsTab| {
            button(text(name).size(fs))
                .width(Length::Fill)
                .padding([6, 10])
                .on_press(Message::SettingsTab(target))
                .style(style::row_button(t, tab == target))
        };
        let danger = t.danger.0;
        let nav = column![
            label(t, "User settings", fs),
            nav_item("My Profile", SettingsTab::Profile),
            space().height(12),
            label(t, "App settings", fs),
            nav_item("Appearance", SettingsTab::Appearance),
            space().height(12),
            divider(t.hover.0),
            button(text("Log Out").size(fs))
                .width(Length::Fill)
                .padding([6, 10])
                .on_press(Message::Logout)
                .style(style::link_button(danger, style::darken(danger, 0.2))),
        ]
        .spacing(4)
        .width(200);

        let content = match tab {
            SettingsTab::Profile => self.profile_settings(s),
            SettingsTab::Appearance => self.appearance_settings(),
        };

        let close = column![
            button(text("X").size(fs * 1.1).font(BOLD))
                .padding([6, 12])
                .on_press(Message::CloseSettings)
                .style(style::icon_button(t)),
            text("ESC").size(fs * 0.7).color(t.muted.0),
        ]
        .spacing(4)
        .align_x(Alignment::Center);

        row![
            container(nav)
                .padding(Padding { top: 60.0, right: 8.0, left: 20.0, bottom: 20.0 })
                .align_x(Alignment::End)
                .width(Length::FillPortion(1))
                .height(Length::Fill)
                .style(bg(t.sidebar.0)),
            container(
                row![
                    scrollable(container(content).padding(Padding { top: 60.0, left: 40.0, right: 20.0, bottom: 40.0 }).max_width(760))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .style(style::scroll(t)),
                    container(close).padding(Padding { top: 60.0, right: 20.0, ..Padding::ZERO }),
                ],
            )
            .width(Length::FillPortion(2))
            .height(Length::Fill)
            .style(bg(t.chat.0)),
        ]
        .into()
    }

    fn profile_settings<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(me) = s.me() else { return space().into() };
        let d = &self.draft;
        let preview_profile = d.to_profile(&me.profile);

        let input = |placeholder: &str, value: &str, on: fn(String) -> Message| {
            text_input(placeholder, value).on_input(on).padding(10).size(fs).style(style::input(t, t.rail.0))
        };
        let color_field = |name: &'static str, value: &str, current: u32, on: fn(String) -> Message| {
            column![
                label(t, name, fs),
                row![
                    container(space()).width(36).height(36).style(card(rgb(current), t.radius * 0.6)),
                    input("#RRGGBB", value, on).width(130),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                style::swatches(current, on),
            ]
            .spacing(8)
        };

        let rings = AvatarRing::ALL.iter().fold(row![].spacing(6), |r, ring| {
            let selected = d.ring == *ring;
            r.push(
                button(text(ring.to_string()).size(fs * 0.85))
                    .padding([6, 10])
                    .on_press(Message::Draft(DraftField::Ring(*ring)))
                    .style(if selected {
                        Box::new(style::solid_button(t.accent.0, t.radius * 0.6)) as Box<dyn Fn(&iced::Theme, button::Status) -> button::Style>
                    } else {
                        Box::new(style::solid_button(t.floating.0, t.radius * 0.6))
                    }),
            )
        });

        let changed = preview_profile != me.profile;
        let form = column![
            column![label(t, "Display name", fs), input(&me.username, &d.display_name, |s| Message::Draft(DraftField::DisplayName(s)))].spacing(8),
            column![label(t, "Custom status", fs), input("What are you up to?", &d.status, |s| Message::Draft(DraftField::Status(s)))].spacing(8),
            column![label(t, "About me", fs), input("Tell your friends about yourself", &d.bio, |s| Message::Draft(DraftField::Bio(s)))].spacing(8),
            column![
                label(t, "Avatar symbol", fs),
                input("1-2 characters (defaults to your initial)", &d.glyph, |s| Message::Draft(DraftField::Glyph(s))),
            ]
            .spacing(8),
            color_field("Name and avatar colour", &d.accent, preview_profile.accent_color, |s| Message::Draft(DraftField::Accent(s))),
            color_field("Banner colour", &d.banner, preview_profile.banner_color, |s| Message::Draft(DraftField::Banner(s))),
            column![label(t, "Avatar decoration", fs), rings].spacing(8),
            color_field("Decoration colour", &d.ring_color, preview_profile.ring_color, |s| Message::Draft(DraftField::RingColor(s))),
            row![
                button(text("Save Changes").size(fs))
                    .padding([8, 16])
                    .on_press_maybe(changed.then_some(Message::SaveProfile))
                    .style(style::solid_button(t.online.0, t.radius * 0.6)),
                button(text("Reset").size(fs))
                    .padding([8, 16])
                    .on_press_maybe(changed.then_some(Message::ResetProfile))
                    .style(style::link_button(t.text.0, t.header.0)),
            ]
            .spacing(8),
        ]
        .spacing(20)
        .width(Length::Fill);

        column![
            text("My Profile").size(fs * 1.4).font(BOLD).color(t.header.0),
            text("Everything here is free. Your friends see changes as soon as you save.").size(fs * 0.9).color(t.muted.0),
            row![
                form,
                column![
                    label(t, "Preview", fs),
                    style::profile_card(t, &me.username, &preview_profile, me.is_owner, true, fs),
                ]
                .spacing(8),
            ]
            .spacing(32),
        ]
        .spacing(16)
        .into()
    }

    fn appearance_settings(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();

        let tiles = self.themes.iter().fold(row![].spacing(12), |r, theme| {
            r.push(theme_tile(t, theme, theme.name == self.theme.name, fs))
        });

        let mut col = column![
            text("Appearance").size(fs * 1.4).font(BOLD).color(t.header.0),
            label(t, "Theme", fs),
            tiles.wrap().vertical_spacing(12),
            label(t, "Custom themes", fs),
            text(match themes_dir() {
                Some(dir) => format!(
                    "Drop .toml theme files into {}. An example.toml is there to copy. Then press Reload.",
                    dir.display()
                ),
                None => "Custom themes aren't supported on this system.".into(),
            })
            .size(fs * 0.9)
            .color(t.text.0),
            button(text("Reload themes").size(fs))
                .padding([8, 16])
                .on_press(Message::ReloadThemes)
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        ]
        .spacing(14);

        for err in &self.theme_errors {
            col = col.push(text(format!("Couldn't load {err}")).size(fs * 0.85).color(t.danger.0));
        }

        col = col
            .push(label(t, "Text size", fs))
            .push(
                row![
                    slider(0.8..=1.5, self.settings.text_scale, Message::TextScale).step(0.05_f32).width(300),
                    text(format!("{:.0}%", self.settings.text_scale * 100.0)).size(fs * 0.9).color(t.text.0),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .push(label(t, "Message display", fs))
            .push(
                toggler(self.settings.compact)
                    .label("Compact mode (one line per message)")
                    .on_toggle(Message::ToggleCompact)
                    .text_size(fs),
            );
        col.into()
    }
}

/// A clickable miniature of a theme's layout.
fn theme_tile<'a>(current: &AppTheme, theme: &'a AppTheme, selected: bool, fs: f32) -> Element<'a, Message> {
    let block = |c: Color, w: f32, h: f32, r: f32| container(space()).width(w).height(h).style(card(c, r));
    let chat_lines = column![
        block(theme.accent.0, 40.0, 6.0, 3.0),
        block(theme.text.0, 70.0, 5.0, 3.0),
        block(theme.muted.0, 54.0, 5.0, 3.0),
        space().height(Length::Fill),
        block(theme.floating.0, 92.0, 12.0, 4.0),
    ]
    .spacing(5);
    let mini = row![
        container(block(theme.accent.0, 14.0, 14.0, 7.0)).padding(5).height(Length::Fill).style(bg(theme.rail.0)),
        container(column![block(theme.muted.0, 30.0, 5.0, 3.0), block(theme.selected.0, 34.0, 7.0, 3.0)].spacing(5))
            .padding(6)
            .height(Length::Fill)
            .style(bg(theme.sidebar.0)),
        container(chat_lines).padding(6).width(Length::Fill).height(Length::Fill).style(bg(theme.chat.0)),
    ]
    .height(72)
    .width(160);

    let border_color = if selected { current.accent.0 } else { current.hover.0 };
    let radius = current.radius;
    button(
        column![
            container(mini).clip(true).style(move |_| container::Style {
                border: Border { radius: radius.into(), width: 2.0, color: border_color },
                ..Default::default()
            }),
            text(theme.name.clone()).size(fs * 0.9).color(if selected { current.header.0 } else { current.text.0 }),
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .padding(0)
    .on_press(Message::PickTheme(theme.name.clone()))
    .style(|_, _| button::Style::default())
    .into()
}

/// Wrap a button style so offline members appear faded.
fn dimmed(
    inner: impl Fn(&iced::Theme, button::Status) -> button::Style,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut s = inner(theme, status);
        if !matches!(status, button::Status::Hovered) {
            s.text_color = with_alpha(s.text_color, 0.5);
        }
        s
    }
}

fn local(ms: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

fn short_time(ms: i64) -> String {
    local(ms).map(|d| d.format("%-I:%M %p").to_string()).unwrap_or_default()
}

fn long_time(ms: i64) -> String {
    let Some(d) = local(ms) else { return String::new() };
    let today = Local::now().date_naive();
    if d.date_naive() == today {
        d.format("Today at %-I:%M %p").to_string()
    } else if Some(d.date_naive()) == today.pred_opt() {
        d.format("Yesterday at %-I:%M %p").to_string()
    } else {
        d.format("%m/%d/%Y %-I:%M %p").to_string()
    }
}
