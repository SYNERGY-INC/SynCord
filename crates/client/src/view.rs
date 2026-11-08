//! All screens. Layout follows the familiar four-column chat layout:
//! server bar | channel list | messages | member list.

use chrono::{Local, TimeZone};
use iced::widget::{
    Id, button, center, column, container, mouse_area, opaque, pick_list, row, scrollable, slider, space, stack, text,
    text_input, toggler,
};
use iced::{Alignment, Border, Color, Element, Length, Padding};
use syncord_proto::{AvatarRing, ChannelKind, Message as ChatMessage, User, VoiceState};

use crate::app::{
    App, COMPOSER_ID, ChannelInfo, DEFAULT_DEVICE, DraftField, MESSAGES_ID, Message, Overlay, PickerTarget, Session,
    SettingsTab, View,
};
use crate::icons;
use crate::style::{self, BOLD, SEMIBOLD, bg, card, divider, label};
use crate::theme::{AppTheme, contrast_text, rgb, themes_dir, with_alpha};

/// Messages from the same person within this window are grouped under one header.
const GROUP_WINDOW_MS: i64 = 7 * 60 * 1000;

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        let main = self.view_main();
        let main = match self.incoming_call() {
            Some(call) => stack![
                main,
                container(crate::widgets::pop(call, self.pop(self.call_popped)))
                    .padding(20)
                    .align_right(Length::Fill)
                    .align_bottom(Length::Fill)
            ]
            .into(),
            None => main,
        };
        match self.update_popup() {
            Some(popup) => stack![
                main,
                opaque(
                    center(opaque(popup))
                        .style(bg(Color::from_rgba(0.0, 0.0, 0.0, 0.6)))
                        .width(Length::Fill)
                        .height(Length::Fill)
                )
            ]
            .into(),
            None => main,
        }
    }

    fn view_main(&self) -> Element<'_, Message> {
        let Some(session) = &self.session else { return self.login_view() };

        let base: Element<'_, Message> = match self.settings_tab {
            Some(tab) => self.settings_view(session, tab),
            None => self.with_message_menu(self.main_view(session)),
        };

        match &self.overlay {
            None => base,
            Some(overlay) => {
                let content = match overlay {
                    Overlay::Profile(id) => match session.users.get(id) {
                        Some(u) => self.profile_popup(session, u),
                        None => return base,
                    },
                    Overlay::NewChannel { name, topic, kind } => self.new_channel_dialog(name, topic, *kind),
                    Overlay::ConfirmDelete(id) => self.confirm_delete_dialog(session, *id),
                    Overlay::Image(a) => self.image_viewer(a),
                    Overlay::NewDirect => self.new_direct_dialog(session),
                    Overlay::Crop => self.crop_dialog(),
                    Overlay::Switcher { query, index } => self.switcher_dialog(session, query, *index),
                    Overlay::Shortcuts => self.shortcuts_dialog(),
                    Overlay::StatusPicker => self.status_picker(session),
                };
                let dim = if matches!(overlay, Overlay::Image(_)) { 0.88 } else { 0.6 };
                // The backdrop fades in while the dialog pops up.
                let dim = dim * ((self.pop(self.popped) - 0.9) / 0.1).clamp(0.0, 1.0);
                let content = crate::widgets::pop(content, self.pop(self.popped));
                let backdrop = mouse_area(
                    center(opaque(content))
                        .style(bg(Color::from_rgba(0.0, 0.0, 0.0, dim)))
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
            (
                "Create an account",
                "Ask whoever runs the server for the invite code.",
                "Continue",
                "Already have an account? Log in",
            )
        } else {
            ("Welcome back!", "We're so excited to see you again!", "Log In", "Need an account? Register")
        };

        let mut form = column![
            column![text(title).size(fs * 1.6).font(BOLD).color(t.header.0), text(subtitle).size(fs).color(t.muted.0),]
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
        if let Some(seen) = &self.identity_changed {
            let mut col = column![].spacing(8);
            if !seen.is_empty() {
                col = col.push(
                    text(format!("New fingerprint: {seen}"))
                        .size(fs * 0.75)
                        .font(iced::Font::MONOSPACE)
                        .color(t.muted.0),
                );
                col = col.push(
                    text("The server owner can find the right one at the top of syncord-server.log.")
                        .size(fs * 0.8)
                        .color(t.muted.0),
                );
            }
            col = col.push(
                button(text("Trust the new identity").size(fs * 0.9))
                    .padding([8, 14])
                    .on_press(Message::TrustNewIdentity)
                    .style(style::solid_button(t.danger.0, t.radius * 0.6)),
            );
            form = form.push(col);
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
        if s.view == View::Home {
            let mut layout = row![self.server_rail(s), self.home_sidebar(s), self.chat_area(s)];
            if self.search.open {
                layout = layout.push(self.search_panel(s));
            }
            return layout.height(Length::Fill).into();
        }
        let mut layout = row![self.server_rail(s), self.channel_sidebar(s), self.chat_area(s)];
        if self.search.open {
            layout = layout.push(self.search_panel(s));
        } else if self.settings.show_members {
            layout = layout.push(self.member_list(s));
        }
        layout.height(Length::Fill).into()
    }

    fn server_rail<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let header = t.header.0;
        // White "pill" on the left edge marks what's selected (taller) or has unread messages (a dot).
        let pill = move |height: f32| {
            container(space()).width(4).height(height).style(move |_| container::Style {
                background: Some(header.into()),
                border: Border { radius: iced::border::right(4.0), ..Default::default() },
                ..Default::default()
            })
        };
        let tile = |content: Element<'a, Message>, color: Color, selected: bool, msg: Message| {
            button(container(content).center(48).style(move |_| container::Style {
                background: Some(color.into()),
                border: Border { radius: if selected { 16.0 } else { 24.0 }.into(), ..Default::default() },
                ..Default::default()
            }))
            .padding(0)
            .on_press(msg)
            .style(|_, _| button::Style::default())
        };

        // Direct messages.
        let home_selected = s.view == View::Home;
        let home_color = if home_selected { t.accent.0 } else { t.sidebar.0 };
        let home_icon =
            icons::icon(&icons::DIRECT, 26.0, if home_selected { contrast_text(t.accent.0) } else { t.text.0 });
        let mut home = stack![tile(home_icon, home_color, home_selected, Message::ShowHome)];
        // Pings first (like Discord), else how many conversations have something new.
        let unread = match s.direct_mentions() {
            0 => s.unread_directs() as u32,
            n => n,
        };
        if unread > 0 {
            let danger = t.danger.0;
            let badge = container(text(unread.min(99).to_string()).size(fs * 0.7).font(BOLD).color(Color::WHITE))
                .padding([1, 5])
                .style(move |_| container::Style {
                    background: Some(danger.into()),
                    border: Border { radius: 9.0.into(), width: 3.0, color: Color::TRANSPARENT },
                    ..Default::default()
                });
            home = home.push(container(badge).width(48).height(48).align_x(Alignment::End).align_y(Alignment::End));
        }
        let home_pill: Element<'a, Message> = if home_selected { pill(40.0).into() } else { space().width(4).into() };

        // The community.
        let initials: String =
            s.community.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect::<String>().to_uppercase();
        let server_selected = s.view == View::Server;
        let accent = t.accent.0;
        let server_unread = s.channels.iter().any(|c| c.kind == ChannelKind::Text && s.is_unread(c.id));
        let label = text(initials).size(fs * 1.1).font(BOLD).color(contrast_text(accent));
        let server_pings = s.server_mentions();
        let server_pill: Element<'a, Message> = if server_selected {
            pill(40.0).into()
        } else if server_unread {
            pill(8.0).into()
        } else {
            space().width(4).into()
        };

        let entry = |p: Element<'a, Message>, content: Element<'a, Message>| {
            row![p, space().width(8), content].align_y(Alignment::Center)
        };
        container(
            column![
                entry(home_pill, home.into()),
                container(divider(t.hover.0)).width(32),
                entry(server_pill, {
                    let mut t2 = stack![tile(label.into(), accent, server_selected, Message::ShowServer)];
                    // A speaker while anyone is talking in one of the server's voice channels.
                    let voice_active = s.voice.values().any(|v| s.channels.iter().any(|c| c.id == v.channel_id));
                    if voice_active {
                        let green = t.online.0;
                        let rail = t.rail.0;
                        let speaker =
                            container(icons::icon(&icons::SPEAKER, 11.0, Color::WHITE)).center(20).style(move |_| {
                                container::Style {
                                    background: Some(green.into()),
                                    border: Border { radius: 10.0.into(), width: 3.0, color: rail },
                                    ..Default::default()
                                }
                            });
                        t2 = t2.push(
                            container(speaker).width(48).height(48).align_x(Alignment::Start).align_y(Alignment::End),
                        );
                    }
                    if server_pings > 0 {
                        t2 = t2.push(
                            container(mention_badge(t, server_pings, fs))
                                .width(48)
                                .height(48)
                                .align_x(Alignment::End)
                                .align_y(Alignment::End),
                        );
                    }
                    t2.into()
                }),
            ]
            .spacing(10)
            .align_x(Alignment::Center),
        )
        .padding([12, 0])
        .width(72)
        .height(Length::Fill)
        .style(bg(t.rail.0))
        .into()
    }

    fn category<'a>(&'a self, s: &'a Session, title: &'static str, kind: ChannelKind) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut category = row![label(t, title, fs), space::horizontal()].align_y(Alignment::Center);
        if s.can(syncord_proto::perms::MANAGE_CHANNELS) {
            category = category.push(
                button(text("+").size(fs * 1.2))
                    .padding([0, 6])
                    .on_press(Message::OpenNewChannel(kind))
                    .style(style::link_button(t.muted.0, t.header.0)),
            );
        }
        container(category).padding(Padding { left: 8.0, top: 16.0, right: 4.0, bottom: 4.0 }).into()
    }

    fn channel_sidebar<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();

        let header = container(text(s.community.clone()).size(fs * 1.05).font(BOLD).color(t.header.0))
            .padding([0, 16])
            .height(48)
            .width(Length::Fill)
            .align_y(Alignment::Center);

        let mut list = column![self.category(s, "Text channels", ChannelKind::Text)].spacing(2);
        for ch in s.channels.iter().filter(|c| c.kind == ChannelKind::Text) {
            let selected = s.current == Some(ch.id);
            let unread = s.is_unread(ch.id) && !selected;
            let name_color = if unread && !selected { Some(t.header.0) } else { None };
            let mut name = text(ch.name.clone()).size(fs);
            if unread {
                name = name.font(SEMIBOLD);
            }
            if let Some(c) = name_color {
                name = name.color(c);
            }
            let mut line = row![text("#").size(fs * 1.25).color(t.muted.0), name.width(Length::Fill)]
                .spacing(6)
                .align_y(Alignment::Center);
            if s.mention_count(ch.id) > 0 && !selected {
                line = line.push(mention_badge(t, s.mention_count(ch.id), fs));
            }
            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding([6, 8])
                    .on_press(Message::SelectChannel(ch.id))
                    .style(style::row_button(t, selected)),
            );
        }

        list = list.push(self.category(s, "Voice channels", ChannelKind::Voice));
        for ch in s.channels.iter().filter(|c| c.kind == ChannelKind::Voice) {
            let connected = self.voice.as_ref().is_some_and(|v| v.channel_id == ch.id);
            let line = row![icons::icon(&icons::SPEAKER, fs * 1.2, t.muted.0), text(ch.name.clone()).size(fs)]
                .spacing(6)
                .align_y(Alignment::Center);
            let mut entry = button(line).width(Length::Fill).padding([6, 8]).style(style::row_button(t, connected));
            if !connected {
                entry = entry.on_press(Message::JoinVoice(ch.id));
            }
            list = list.push(entry);

            // Who's in this channel.
            let mut members: Vec<(&User, &VoiceState)> = s
                .voice
                .values()
                .filter(|v| v.channel_id == ch.id)
                .filter_map(|v| Some((s.users.get(&v.user_id)?, v)))
                .collect();
            members.sort_by_key(|(u, _)| u.name().to_lowercase());
            for (u, v) in members {
                list = list.push(self.voice_member(u, v));
            }
        }

        container(column![
            header,
            divider(style::darken(t.sidebar.0, 0.25)),
            scrollable(container(list).padding([0, 8])).height(Length::Fill).style(style::scroll(t)),
            self.voice_panel(s),
            self.user_panel(s),
        ])
        .width(240)
        .height(Length::Fill)
        .style(bg(t.sidebar.0))
        .into()
    }

    /// One person listed under a voice channel, with a ring while they talk.
    fn voice_member<'a>(&'a self, u: &'a User, v: &VoiceState) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let speaking = self.speaking.contains(&u.id);
        let ring = if speaking { t.online.0 } else { Color::TRANSPARENT };
        let pic = container(self.avatar(u, 22.0)).padding(2).style(move |_| container::Style {
            border: Border { radius: 14.0.into(), width: 2.0, color: ring },
            ..Default::default()
        });
        let mut line = row![pic, text(truncate(u.name(), 16)).size(fs * 0.9).width(Length::Fill)]
            .spacing(8)
            .align_y(Alignment::Center);
        if v.muted && !v.deafened {
            line = line.push(icons::icon(&icons::MIC_OFF, fs, t.danger.0));
        }
        if v.deafened {
            line = line.push(icons::icon(&icons::HEADPHONES_OFF, fs, t.danger.0));
        }
        button(line)
            .width(Length::Fill)
            .padding(Padding { left: 28.0, right: 8.0, top: 2.0, bottom: 2.0 })
            .on_press(Message::ShowProfile(u.id))
            .style(style::row_button(t, false))
            .into()
    }

    /// "Voice Connected" box above the user panel while in a call.
    pub(crate) fn voice_panel<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let (channel_id, connected) = match (&self.voice, self.voice_joining) {
            (Some(v), _) => (v.channel_id, true),
            (None, Some(id)) => (id, false),
            (None, None) => return space().into(),
        };
        let dm_name =
            s.direct(channel_id).and_then(|d| s.users.get(&d.user_id)).map(|u| format!("Call with {}", u.name()));
        let channel =
            s.channels.iter().find(|c| c.id == channel_id).map(|c| c.name.clone()).or(dm_name).unwrap_or_default();
        let channel = channel.as_str();
        let (title, color) = if connected { ("Voice Connected", t.online.0) } else { ("Connecting...", rgb(0xF0B232)) };
        let route = if !connected {
            String::new()
        } else if self.voice_status.udp {
            format!("{channel} · direct (UDP)")
        } else {
            format!("{channel} · relayed (TCP)")
        };

        let mut info = column![
            row![icons::icon(&icons::SIGNAL, fs, color), text(title).size(fs * 0.9).font(BOLD).color(color)]
                .spacing(6)
                .align_y(Alignment::Center),
            text(if route.is_empty() { channel.to_string() } else { route }).size(fs * 0.75).color(t.muted.0),
        ]
        .spacing(2)
        .width(Length::Fill);
        if let Some(problem) = &self.voice_status.problem {
            info = info.push(text(problem.clone()).size(fs * 0.7).color(rgb(0xF0B232)));
        }

        let content = row![
            info,
            button(icons::icon(&icons::HANG_UP, fs * 1.3, t.header.0))
                .padding(6)
                .on_press(Message::LeaveVoice)
                .style(style::icon_button(t)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        container(column![
            divider(style::darken(t.user_panel.0, 0.2)),
            container(content).padding([8, 10]).width(Length::Fill)
        ])
        .width(Length::Fill)
        .style(bg(t.user_panel.0))
        .into()
    }

    pub(crate) fn user_panel<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(me) = s.me() else { return space().into() };
        let status = if self.reconnecting {
            "Reconnecting...".to_string()
        } else if let Some(a) =
            self.effective_activity().filter(|_| self.effective_status() != syncord_proto::Status::Invisible)
        {
            truncate(&format!("{} {}", a.kind.verb(), a.name), 14)
        } else if !me.profile.status_text.is_empty() {
            truncate(&me.profile.status_text, 12)
        } else {
            self.effective_status().label().to_string()
        };

        let toggle = |handle: &'static std::sync::LazyLock<iced::widget::svg::Handle>, on: bool, msg: Message| {
            let color = if on { t.danger.0 } else { t.muted.0 };
            button(icons::icon(handle, fs * 1.25, color)).padding(5).on_press(msg).style(style::icon_button(t))
        };
        let mic = if self.muted || self.deafened { &icons::MIC_OFF } else { &icons::MIC_ON };
        let ears = if self.deafened { &icons::HEADPHONES_OFF } else { &icons::HEADPHONES_ON };

        // Click your name or avatar to set your status, like Discord.
        let who = button(
            row![
                self.avatar_status(me, 32.0, !self.reconnecting, t.user_panel.0),
                column![
                    text(truncate(me.name(), 11)).size(fs * 0.9).font(SEMIBOLD).color(t.header.0),
                    text(status).size(fs * 0.75).color(t.muted.0),
                ]
                .spacing(1),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding([2, 4])
        .on_press(Message::OpenStatusPicker)
        .style(style::row_button(t, false));
        let content = row![
            who,
            toggle(mic, self.muted || self.deafened, Message::ToggleMute),
            toggle(ears, self.deafened, Message::ToggleDeafen),
            button(icons::icon(&icons::GEAR, fs * 1.25, t.muted.0))
                .padding(5)
                .on_press(Message::OpenSettings)
                .style(style::icon_button(t)),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        container(content).padding([10, 8]).width(Length::Fill).style(bg(t.user_panel.0)).into()
    }

    fn chat_area<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let channel = s.current_info();
        let is_direct = channel.as_ref().is_some_and(|c| c.direct_with.is_some());

        // Header: "# name | topic" ............ [Members]
        let mut header = row![].spacing(10).align_y(Alignment::Center);
        if let Some(ch) = &channel {
            header = header
                .push(text(if is_direct { "@" } else { "#" }).size(fs * 1.5).color(t.muted.0))
                .push(text(ch.name.to_owned()).size(fs * 1.05).font(BOLD).color(t.header.0));
            if !ch.topic.is_empty() {
                header = header
                    .push(container(space()).width(1).height(24).style(bg(t.hover.0)))
                    .push(text(ch.topic.to_owned()).size(fs * 0.9).color(t.muted.0));
            }
        }
        header = header.push(space::horizontal());
        if channel.is_some() {
            header = header.push(self.search_box());
        }
        if let Some(ch) = channel
            .as_ref()
            .filter(|_| s.can(syncord_proto::perms::MANAGE_CHANNELS) && s.channels.len() > 1 && !is_direct)
        {
            header = header.push(
                button(text("Delete channel").size(fs * 0.8).wrapping(iced::widget::text::Wrapping::None))
                    .padding([4, 8])
                    .on_press(Message::AskDeleteChannel(ch.id))
                    .style(style::icon_button(t)),
            );
        }
        if let Some(ch) = channel.as_ref().filter(|_| is_direct) {
            let in_call =
                self.voice.as_ref().is_some_and(|v| v.channel_id == ch.id) || self.voice_joining == Some(ch.id);
            if !in_call {
                header = header.push(
                    button(icons::icon(&icons::PHONE, fs * 1.3, t.text.0))
                        .padding([4, 8])
                        .on_press(Message::JoinVoice(ch.id))
                        .style(style::icon_button(t)),
                );
            }
        }
        // While searching, the results take the members' place.
        if !is_direct && !self.search.open {
            header = header.push(
                button(
                    text(if self.settings.show_members { "Hide members" } else { "Members" })
                        .size(fs * 0.8)
                        .wrapping(iced::widget::text::Wrapping::None),
                )
                    .padding([4, 8])
                    .on_press(Message::ToggleMembers)
                    .style(style::icon_button(t)),
            );
        }
        let header = container(header).padding([0, 16]).height(48).width(Length::Fill).align_y(Alignment::Center);

        let mut col = column![header, divider(style::darken(t.chat.0, 0.25))];
        if let Some(ch) = channel.as_ref().filter(|_| is_direct)
            && let Some(bar) = self.call_bar(s, ch.id)
        {
            col = col.push(bar);
        }

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

        let has_channel = channel.is_some();
        col = col.push(self.message_list(s, channel.as_ref()));
        if channel.as_ref().and_then(|c| s.chans.get(&c.id)).is_some_and(|st| st.detached) {
            col = col.push(self.older_bar());
        }
        if let Some(ch) = channel {
            col = col.push(self.composer(s, ch));
        }
        let chat = container(col).width(Length::Fill).height(Length::Fill).style(bg(t.chat.0));
        // Always a stack, so opening the picker doesn't reset the chat's scroll position or focus.
        let picker: Element<'a, Message> = match self.emoji_picker_view().filter(|_| has_channel) {
            Some(picker) => container(opaque(crate::widgets::pop(picker, self.pop(self.picker_popped))))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::End)
                .padding(Padding { right: 16.0, bottom: 76.0, top: 56.0, left: 16.0 })
                .into(),
            None => space().into(),
        };
        stack![chat, picker].into()
    }

    fn message_list<'a>(&'a self, s: &'a Session, channel: Option<&ChannelInfo<'a>>) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(channel) = channel else {
            let hint = if s.view == View::Home {
                "Pick a conversation, or click + to message someone."
            } else {
                "No channels yet."
            };
            return center(text(hint).color(t.muted.0)).into();
        };
        let state = s.chans.get(&channel.id);
        let messages: &[ChatMessage] = state.map(|st| st.messages.as_slice()).unwrap_or(&[]);

        let mut list = column![].padding(Padding { top: 16.0, bottom: 16.0, left: 0.0, right: 0.0 });

        match state {
            Some(st) if st.loaded && st.reached_start => {
                let (badge, title, line): (Element<'a, Message>, String, String) = match channel.direct_with {
                    Some(u) => (
                        self.avatar(u, 68.0),
                        u.name().to_owned(),
                        format!("This is the beginning of your direct messages with {}.", u.name()),
                    ),
                    None => (
                        container(text("#").size(fs * 2.4).color(t.header.0))
                            .center(68)
                            .style(card(t.floating.0, 34.0))
                            .into(),
                        format!("Welcome to #{}!", channel.name),
                        format!("This is the start of the #{} channel.", channel.name),
                    ),
                };
                list = list.push(
                    container(
                        column![
                            badge,
                            text(title).size(fs * 2.0).font(BOLD).color(t.header.0),
                            text(line).size(fs).color(t.muted.0),
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
                && prev.is_some_and(|p| {
                    p.author_id == m.author_id
                        && p.kind.is_normal()
                        && m.kind.is_normal()
                        && m.created_at - p.created_at < GROUP_WINDOW_MS
                });
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
        if !m.kind.is_normal() {
            return self.call_note(s, m);
        }
        if self.hides(s, m.author_id) && !self.revealed.contains(&m.id) {
            return self.hidden_message(s, m);
        }
        let name = author.map(User::name).unwrap_or("Unknown user");
        // Like Discord: your highest coloured role, else your profile colour.
        let name_color = match (s.role_color(m.author_id), author) {
            (Some(c), _) => rgb(c),
            (None, Some(u)) => rgb(u.profile.accent_color),
            (None, None) => t.header.0,
        };
        let hovered = self.hovered == Some(m.id);
        let editing = self.editing.as_ref().filter(|(id, _)| *id == m.id);

        let jumbo = crate::emoji::is_jumbo(&m.content);
        let body_text = self.message_text(s, &m.content, if jumbo { fs * 2.6 } else { fs });
        let selecting = self.selecting.as_ref().filter(|sel| sel.message_id == m.id);
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
                let mut col = column![].spacing(6);
                if let Some(sel) = selecting {
                    col = col.push(self.selectable_body(sel, fs));
                } else if !m.content.is_empty() {
                    let mut r = row![body_text].spacing(6).align_y(Alignment::End);
                    if m.edited_at.is_some() {
                        r = r.push(text("(edited)").size(fs * 0.7).color(t.muted.0));
                    }
                    col = col.push(r.wrap());
                }
                if let Some(files) = self.attachments_view(m) {
                    col = col.push(files);
                }
                if let Some(embeds) = self.embeds_view(m) {
                    col = col.push(embeds);
                }
                if let Some(reactions) = self.reactions_view(s, m) {
                    col = col.push(reactions);
                }
                col.into()
            }
        };

        let content: Element<'a, Message> = if self.settings.compact {
            row![
                text(short_time(m.created_at)).size(fs * 0.75).color(t.muted.0).width(64),
                self.with_user_menu(
                    button(text(name.to_owned()).size(fs).font(SEMIBOLD).color(name_color))
                        .padding(0)
                        .on_press(Message::ShowProfile(m.author_id))
                        .style(style::link_button(name_color, name_color)),
                    m.author_id
                ),
                container(body).width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Start)
            .into()
        } else if grouped {
            row![
                container(if hovered {
                    text(short_time(m.created_at))
                        .size(fs * 0.68)
                        .color(t.muted.0)
                        .wrapping(iced::widget::text::Wrapping::None)
                } else {
                    text("")
                })
                .width(40)
                .align_x(Alignment::End)
                .padding(Padding { top: 3.0, ..Padding::ZERO }),
                container(body).width(Length::Fill),
            ]
            .spacing(16)
            .into()
        } else {
            let pic: Element<'a, Message> = match author {
                Some(u) => self.with_user_menu(
                    button(self.avatar(u, 40.0))
                        .padding(0)
                        .on_press(Message::ShowProfile(u.id))
                        .style(style::link_button(t.text.0, t.text.0)),
                    u.id,
                ),
                None => container(space()).width(40).height(40).style(card(t.hover.0, 20.0)).into(),
            };
            row![
                pic,
                column![
                    row![
                        self.with_user_menu(
                            button(text(name.to_owned()).size(fs).font(SEMIBOLD).color(name_color))
                                .padding(0)
                                .on_press(Message::ShowProfile(m.author_id))
                                .style(style::link_button(name_color, name_color)),
                            m.author_id
                        ),
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

        // Hover toolbar: react for everyone; edit/delete your own (the owner can delete in server channels).
        let mine = Some(m.author_id) == Some(s.me);
        let in_server = s.channels.iter().any(|c| c.id == m.channel_id);
        let mut line = row![container(content).width(Length::Fill)].align_y(Alignment::Start);
        if hovered && editing.is_none() && selecting.is_none() {
            let mut tools = row![
                button(icons::icon(&icons::SMILE, fs * 1.1, t.muted.0))
                    .padding([3, 6])
                    .on_press(Message::OpenEmojiPicker(PickerTarget::Reaction(m.id)))
                    .style(style::icon_button(t))
            ]
            .spacing(2)
            .align_y(Alignment::Center);
            if mine {
                tools = tools.push(
                    button(text("Edit").size(fs * 0.8))
                        .padding([2, 8])
                        .on_press(Message::StartEdit(m.id))
                        .style(style::icon_button(t)),
                );
            }
            let danger = t.danger.0;
            if mine || (s.can(syncord_proto::perms::MANAGE_MESSAGES) && in_server) {
                tools = tools.push(
                    button(text("Delete").size(fs * 0.8))
                        .padding([2, 8])
                        .on_press(Message::DeleteMessage(m.id))
                        .style(style::link_button(danger, style::darken(danger, 0.2))),
                );
            }
            tools = tools.push(
                button(icons::icon(&icons::MORE, fs * 1.1, t.muted.0))
                    .padding([3, 6])
                    .on_press(Message::OpenMessageMenu(m.id))
                    .style(style::icon_button(t)),
            );
            line = line.push(container(tools).padding(2).style(card(t.floating.0, t.radius * 0.6)));
        }

        // Messages that ping you get a gold tint and a bar on the left, like Discord.
        // The message you jumped to from search is tinted for a moment.
        let pinged = s.mentions_me(m);
        let jumped = self.search.highlight == Some(m.id);
        let gold = Color::from_rgb8(0xF0, 0xB2, 0x32);
        let hover_bg = match (jumped, pinged, hovered) {
            (true, _, _) => Some(with_alpha(t.accent.0, 0.2)),
            (_, true, true) => Some(with_alpha(gold, 0.14)),
            (_, true, false) => Some(with_alpha(gold, 0.09)),
            (_, false, true) => Some(with_alpha(style::darken(t.chat.0, 0.12), 1.0)),
            (_, false, false) => None,
        };
        let bar_color = if jumped {
            t.accent.0
        } else if pinged {
            gold
        } else {
            Color::TRANSPARENT
        };
        let bar = container(space())
            .width(2)
            .height(Length::Fill)
            .style(move |_| container::Style { background: Some(bar_color.into()), ..Default::default() });
        mouse_area(
            container(row![bar, container(line).padding([2, 14]).width(Length::Fill)])
                .width(Length::Fill)
                .style(move |_| container::Style { background: hover_bg.map(Into::into), ..Default::default() }),
        )
        .on_enter(Message::HoverMessage(Some(m.id)))
        .on_exit(Message::HoverMessage(None))
        .on_right_press(Message::OpenMessageMenu(m.id))
        .on_double_click(Message::SelectMessageText(m.id))
        .into()
    }

    fn composer<'a>(&'a self, s: &'a Session, ch: ChannelInfo<'a>) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let placeholder =
            if ch.direct_with.is_some() { format!("Message @{}", ch.name) } else { format!("Message #{}", ch.name) };
        let suggestions = self.composer_suggestions().len();
        let empty = self.composer.is_empty();
        let (text_color, muted, accent) = (t.text.0, t.muted.0, t.accent.0);
        let input = iced::widget::text_editor(&self.editor)
            .id(Id::new(COMPOSER_ID))
            .placeholder(placeholder)
            .on_action(Message::EditorAction)
            .key_binding(move |kp| crate::composer::key_binding(kp, suggestions, empty))
            .padding(Padding { top: 13.0, bottom: 13.0, left: 4.0, right: 4.0 })
            .size(fs)
            .height(Length::Shrink)
            .max_height(fs * 15.0)
            // The surrounding box draws the background.
            .style(move |_, _| iced::widget::text_editor::Style {
                background: Color::TRANSPARENT.into(),
                border: Border::default(),
                placeholder: muted,
                value: text_color,
                selection: with_alpha(accent, 0.4),
            });
        let floating = t.floating.0;
        let radius = t.radius;
        let bar = container(
            row![
                button(icons::icon(&icons::ATTACH, fs * 1.4, t.muted.0))
                    .padding(8)
                    .on_press(Message::AttachFiles)
                    .style(style::icon_button(t)),
                input,
                button(container(text("GIF").size(fs * 0.7).font(style::BOLD).color(t.muted.0)).padding([1, 4]).style(
                    move |_| container::Style {
                        border: iced::Border { radius: 4.0.into(), width: 1.5, color: t.muted.0 },
                        ..Default::default()
                    }
                ))
                .padding(8)
                .on_press(Message::OpenGifPicker)
                .style(style::icon_button(t)),
                button(icons::icon(&icons::SMILE, fs * 1.4, t.muted.0))
                    .padding(8)
                    .on_press(Message::OpenEmojiPicker(PickerTarget::Composer))
                    .style(style::icon_button(t)),
            ]
            .spacing(2)
            .align_y(Alignment::End),
        )
        .padding(Padding { top: 0.0, bottom: 3.0, left: 6.0, right: 6.0 })
        .style(move |_| container::Style {
            background: Some(floating.into()),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        });

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

        // The extras live in their own column that's always there, so the message box keeps
        // its place in the widget tree (and its keyboard focus) when they appear.
        let mut extras = column![].spacing(6);
        if let Some(suggestions) = self.suggestions_view() {
            extras = extras.push(suggestions);
        }
        if let Some(pending) = self.pending_view() {
            extras = extras.push(pending);
        }
        column![extras, bar, text(typing_text).size(fs * 0.75).font(SEMIBOLD).color(t.text.0).height(fs * 1.2)]
            .spacing(4)
            .padding(Padding { left: 16.0, right: 16.0, top: 0.0, bottom: 4.0 })
            .into()
    }

    fn member_list<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut users: Vec<&User> = s.users.values().collect();
        users.sort_by_key(|u| u.name().to_lowercase());

        // Like Discord: online people under their highest "show separately" role, then
        // everyone else online, then offline.
        let mut groups: Vec<(String, Vec<&User>, bool)> = Vec::new();
        for r in s.roles.iter().filter(|r| r.hoist) {
            let members: Vec<&User> = users
                .iter()
                .copied()
                .filter(|u| s.online.contains(&u.id) && s.hoisted_role(u.id).is_some_and(|h| h.id == r.id))
                .collect();
            groups.push((r.name.clone(), members, true));
        }
        let rest: Vec<&User> =
            users.iter().copied().filter(|u| s.online.contains(&u.id) && s.hoisted_role(u.id).is_none()).collect();
        groups.push(("Online".into(), rest, true));
        let offline: Vec<&User> = users.iter().copied().filter(|u| !s.online.contains(&u.id)).collect();
        groups.push(("Offline".into(), offline, false));

        let mut list = column![].spacing(2).padding([16, 8]);
        for (title, group, is_online) in groups {
            if group.is_empty() {
                continue;
            }
            list = list.push(container(label(t, format!("{title} — {}", group.len()), fs)).padding([8, 8]));
            for u in group {
                let color = s.role_color(u.id).unwrap_or(u.profile.accent_color);
                let name_color = if is_online { rgb(color) } else { t.muted.0 };
                let mut info = column![text(u.name().to_owned()).size(fs * 0.95).font(SEMIBOLD).color(name_color)];
                // Like Discord: what they're doing wins over their status text.
                if let Some(a) = self.activity_of(s, u.id).filter(|_| is_online) {
                    info = info.push(
                        text(truncate(&format!("{} {}", a.kind.verb(), a.name), 26)).size(fs * 0.75).color(t.muted.0),
                    );
                } else if !u.profile.status_text.is_empty() {
                    info = info.push(text(truncate(&u.profile.status_text, 24)).size(fs * 0.75).color(t.muted.0));
                }
                let row_content = row![self.avatar_status(u, 32.0, is_online, t.sidebar.0), info]
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
                list = list.push(self.with_user_menu(entry, u.id));
            }
        }

        container(scrollable(list).height(Length::Fill).style(style::scroll(t)))
            .width(240)
            .height(Length::Fill)
            .style(bg(t.sidebar.0))
            .into()
    }

    // ------------------------------------------------------------------ dialogs

    /// Profile card, plus a volume slider for people you can hear in voice.
    fn profile_popup<'a>(&'a self, s: &'a Session, u: &'a User) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut profile = column![self.profile_card(s, u, s.online.contains(&u.id), true)].spacing(8).width(320);
        if u.id != s.me {
            profile = profile.push(
                button(center(text(format!("Message {}", u.name())).size(fs * 0.95)).height(fs * 2.4))
                    .width(Length::Fill)
                    .padding(0)
                    .on_press(Message::OpenDirect(u.id))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            );
        }
        let in_my_call =
            self.voice.as_ref().is_some_and(|v| s.voice.get(&u.id).is_some_and(|st| st.channel_id == v.channel_id));
        if u.id == s.me || !in_my_call {
            return profile.into();
        }
        let volume = self.settings.user_volume(u.id);
        let id = u.id;
        let control = container(
            column![
                label(t, "User volume", fs),
                row![
                    slider(0.0..=2.0, volume, move |v| Message::UserVolume(id, v)).step(0.05_f32),
                    text(format!("{:.0}%", volume * 100.0)).size(fs * 0.85).color(t.text.0).width(44),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            ]
            .spacing(8),
        )
        .padding([12, 16])
        .width(320)
        .style(card(t.floating.0, t.radius * 1.5));
        profile.push(control).into()
    }

    fn new_channel_dialog<'a>(&'a self, name: &'a str, topic: &'a str, kind: ChannelKind) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let preview = match kind {
            ChannelKind::Text => syncord_proto::normalize_channel_name(name),
            ChannelKind::Voice | ChannelKind::Direct => syncord_proto::clean_voice_channel_name(name),
        };
        let kind_button = |label_text: &'static str, k: ChannelKind| {
            let selected = k == kind;
            button(text(label_text).size(fs * 0.9))
                .padding([8, 14])
                .on_press(Message::NewChannelKind(k))
                .style(style::solid_button(if selected { t.accent.0 } else { t.floating.0 }, t.radius * 0.6))
        };
        let hint = match (kind, preview.is_empty()) {
            (_, true) => String::new(),
            (ChannelKind::Text, false) => format!("Will appear as #{preview}"),
            (ChannelKind::Voice | ChannelKind::Direct, false) => format!("Will appear as {preview}"),
        };
        let topic_field: Element<'a, Message> = if kind == ChannelKind::Text {
            column![
                label(t, "Topic (optional)", fs),
                text_input("What's this channel about?", topic)
                    .on_input(Message::NewChannelTopic)
                    .on_submit(Message::CreateChannel)
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.rail.0))
            ]
            .spacing(10)
            .into()
        } else {
            space().into()
        };
        let body = column![
            text("Create channel").size(fs * 1.3).font(BOLD).color(t.header.0),
            label(t, "Channel type", fs),
            row![kind_button("# Text", ChannelKind::Text), kind_button("Voice", ChannelKind::Voice)].spacing(8),
            label(t, "Channel name", fs),
            text_input("new-channel", name)
                .id(Id::new("new-channel"))
                .on_input(Message::NewChannelName)
                .on_submit(Message::CreateChannel)
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
            text(hint).size(fs * 0.8).color(t.muted.0),
            topic_field,
            row![
                space::horizontal(),
                button(text("Cancel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
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
                button(text("Cancel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
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
            nav_item("Voice & Audio", SettingsTab::Voice),
            space().height(12),
        ]
        .spacing(4)
        .width(200);
        let mut nav = nav;
        if s.can(syncord_proto::perms::MANAGE_ROLES) {
            nav = nav
                .push(label(t, "Server settings", fs))
                .push(nav_item("Roles", SettingsTab::Roles))
                .push(space().height(12));
        }
        let nav = nav.push(
            column![
                divider(t.hover.0),
                divider(t.hover.0),
                button(text("Log Out").size(fs))
                    .width(Length::Fill)
                    .padding([6, 10])
                    .on_press(Message::Logout)
                    .style(style::link_button(danger, style::darken(danger, 0.2))),
            ]
            .spacing(4),
        );

        let content = match tab {
            SettingsTab::Profile => self.profile_settings(s),
            SettingsTab::Appearance => self.appearance_settings(),
            SettingsTab::Voice => self.voice_settings(),
            SettingsTab::Roles => self.roles_settings(s),
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
            container(row![
                scrollable(
                    container(content)
                        .padding(Padding { top: 60.0, left: 40.0, right: 20.0, bottom: 40.0 })
                        .max_width(760)
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .style(style::scroll(t)),
                container(close).padding(Padding { top: 60.0, right: 20.0, ..Padding::ZERO }),
            ],)
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
                        Box::new(style::solid_button(t.accent.0, t.radius * 0.6))
                            as Box<dyn Fn(&iced::Theme, button::Status) -> button::Style>
                    } else {
                        Box::new(style::solid_button(t.floating.0, t.radius * 0.6))
                    }),
            )
        });

        let changed = preview_profile != me.profile;
        let form = column![
            column![
                label(t, "Display name", fs),
                input(&me.username, &d.display_name, |s| Message::Draft(DraftField::DisplayName(s)))
            ]
            .spacing(8),
            column![
                label(t, "Pronouns", fs),
                input("e.g. she/her, they/them", &d.pronouns, |s| Message::Draft(DraftField::Pronouns(s)))
            ]
            .spacing(8),
            self.picture_buttons(me),
            column![
                label(t, "Custom status", fs),
                input("What are you up to?", &d.status, |s| Message::Draft(DraftField::Status(s)))
            ]
            .spacing(8),
            column![
                label(t, "About me", fs),
                input("Tell your friends about yourself", &d.bio, |s| Message::Draft(DraftField::Bio(s)))
            ]
            .spacing(8),
            column![
                label(t, "Avatar symbol", fs),
                input("1-2 characters (defaults to your initial)", &d.glyph, |s| Message::Draft(DraftField::Glyph(s))),
            ]
            .spacing(8),
            color_field("Name and avatar colour", &d.accent, preview_profile.accent_color, |s| Message::Draft(
                DraftField::Accent(s)
            )),
            color_field("Banner colour", &d.banner, preview_profile.banner_color, |s| Message::Draft(
                DraftField::Banner(s)
            )),
            column![label(t, "Avatar decoration", fs), rings].spacing(8),
            color_field("Decoration colour", &d.ring_color, preview_profile.ring_color, |s| Message::Draft(
                DraftField::RingColor(s)
            )),
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
            text("Everything here is free. Your friends see changes as soon as you save.")
                .size(fs * 0.9)
                .color(t.muted.0),
            row![
                form,
                column![
                    label(t, "Preview", fs),
                    self.profile_card(s, &User { profile: preview_profile.clone(), ..me.clone() }, true, true),
                ]
                .spacing(8),
            ]
            .spacing(32),
        ]
        .spacing(16)
        .into()
    }

    fn voice_settings(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let st = &self.settings;

        let device_picker = |options: &[String], current: &Option<String>, on: fn(String) -> Message| {
            let mut all = vec![DEFAULT_DEVICE.to_string()];
            all.extend(options.iter().cloned());
            let selected = current.clone().unwrap_or_else(|| DEFAULT_DEVICE.to_string());
            pick_list(all, Some(selected), on).text_size(fs * 0.9).padding(10).width(Length::Fill)
        };
        let percent_slider = |value: f32, on: fn(f32) -> Message| {
            row![
                slider(0.0..=2.0, value, on).step(0.05_f32),
                text(format!("{:.0}%", value * 100.0)).size(fs * 0.85).color(t.text.0).width(48),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
        };

        let live = self.voice.is_some() || self.mic_test.is_some();
        let meter = level_meter(t, self.voice_status.input_level_db, st.input_sensitivity, live);

        let test_label = if self.mic_test.is_some() { "Stop Testing" } else { "Let's Check" };
        let mut test_row = row![
            button(text(test_label).size(fs))
                .padding([8, 16])
                .on_press_maybe(self.voice.is_none().then_some(Message::ToggleMicTest))
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        let hint = if self.voice.is_some() {
            "You're in a call, so the meter below is live."
        } else if self.mic_test.is_some() {
            "Say something: you should hear yourself back."
        } else {
            "Having mic trouble? Start a test and say something silly. We'll play your voice back to you."
        };
        test_row = test_row.push(text(hint).size(fs * 0.85).color(t.muted.0).width(Length::Fill));

        let mut col = column![
            text("Voice & Audio").size(fs * 1.4).font(BOLD).color(t.header.0),
            row![
                column![label(t, "Input device", fs), device_picker(&self.input_devices, &st.input_device, Message::InputDevice)]
                    .spacing(8)
                    .width(Length::Fill),
                column![label(t, "Output device", fs), device_picker(&self.output_devices, &st.output_device, Message::OutputDevice)]
                    .spacing(8)
                    .width(Length::Fill),
            ]
            .spacing(20),
            row![
                column![label(t, "Input volume", fs), percent_slider(st.input_gain, Message::InputGain)]
                    .spacing(8)
                    .width(Length::Fill),
                column![label(t, "Output volume", fs), percent_slider(st.output_volume, Message::OutputVolume)]
                    .spacing(8)
                    .width(Length::Fill),
            ]
            .spacing(20),
            label(t, "Mic test", fs),
            test_row,
            divider(t.hover.0),
            label(t, "Input sensitivity", fs),
            text("Your mic only sends sound louder than the marker. Drag it so the bar turns green when you talk but not when you're quiet.")
                .size(fs * 0.85)
                .color(t.muted.0),
            meter,
            slider(-80.0..=-10.0, st.input_sensitivity, Message::Sensitivity).step(1.0_f32).width(360),
            text(format!("Threshold: {:.0} dB", st.input_sensitivity)).size(fs * 0.8).color(t.muted.0),
            divider(t.hover.0),
            label(t, "Voice processing", fs),
            toggler(st.noise_suppression)
                .label("Noise suppression (removes fans, keyboards and background hum)")
                .on_toggle(Message::ToggleNoiseSuppression)
                .text_size(fs),
            text("Tip: use headphones so others don't hear themselves echo through your mic.")
                .size(fs * 0.8)
                .color(t.muted.0),
            divider(t.hover.0),
            label(t, "Sounds", fs),
            toggler(st.dm_sound).label("Play a sound for new direct messages").on_toggle(Message::ToggleDmSound).text_size(fs),
            toggler(st.server_sound)
                .label("Play a sound for new messages in server channels")
                .on_toggle(Message::ToggleServerSound)
                .text_size(fs),
            toggler(st.ping_sound)
                .label("Play a sound when someone mentions you or calls you")
                .on_toggle(Message::TogglePingSound)
                .text_size(fs),
            toggler(st.voice_sounds)
                .label("Play a sound when people join or leave your voice channel")
                .on_toggle(Message::ToggleVoiceSounds)
                .text_size(fs),
            row![
                text("Sound volume").size(fs * 0.9).color(t.text.0),
                slider(0.0..=1.0, st.sound_volume, Message::SoundVolume).step(0.05_f32).width(240),
                text(format!("{:.0}%", st.sound_volume * 100.0)).size(fs * 0.85).color(t.text.0).width(48),
                button(text("Test").size(fs * 0.9))
                    .padding([6, 14])
                    .on_press(Message::TestSound)
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            text("Sounds play on the output device above and are muted while you're deafened.")
                .size(fs * 0.8)
                .color(t.muted.0),
        ]
        .spacing(14);
        if let Some(notice) = &self.notice {
            col = col.push(text(notice.clone()).size(fs * 0.85).color(t.danger.0));
        }
        col.into()
    }

    fn update_popup(&self) -> Option<Element<'_, Message>> {
        use crate::updater::Stage;
        let t = &self.theme;
        let fs = self.fs();
        let in_call = self.voice.is_some() || self.voice_joining.is_some();
        let (title, body, buttons): (String, Element<'_, Message>, Element<'_, Message>) = match &self.update {
            Stage::Ready { version, notes, deadline, .. } => {
                let secs = deadline.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
                let status = if in_call {
                    "SynCord will update as soon as you leave the voice call.".to_string()
                } else {
                    format!("Installing in {secs} second{}... SynCord will reopen by itself.", if secs == 1 { "" } else { "s" })
                };
                // Release notes are Markdown; show them as plain text with bullets.
                let summary: String = notes
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(6)
                    .map(|l| {
                        let l = l.replace("**", "").replace('`', "");
                        match l.trim_start().strip_prefix("- ") {
                            Some(rest) => format!("• {rest}"),
                            None => l,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut col = column![text(status).size(fs).color(t.header.0)].spacing(10);
                if !summary.is_empty() {
                    col = col.push(
                        container(scrollable(text(summary).size(fs * 0.85).color(t.text.0)).height(Length::Shrink))
                            .max_height(160)
                            .padding(10)
                            .width(Length::Fill)
                            .style(card(t.rail.0, t.radius)),
                    );
                }
                let buttons = row![
                    space::horizontal(),
                    button(text("Later").size(fs)).padding([8, 16]).on_press(Message::UpdateLater).style(style::link_button(t.text.0, t.header.0)),
                    button(text("Update now").size(fs))
                        .padding([8, 16])
                        .on_press(Message::UpdateNow)
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                ]
                .spacing(8);
                (format!("Updating SynCord to {version}"), col.into(), buttons.into())
            }
            Stage::Installing(version) => (
                format!("Installing SynCord {version}"),
                text("Just a moment, SynCord will reopen by itself.").size(fs).color(t.text.0).into(),
                space().into(),
            ),
            Stage::Manual { version, dismissed: false, .. } => (
                format!("SynCord {version} is available"),
                text("This copy was installed by your system's package manager, so it can't update itself. Download the new version and open it to install.")
                    .size(fs)
                    .color(t.text.0)
                    .into(),
                row![
                    space::horizontal(),
                    button(text("Close").size(fs)).padding([8, 16]).on_press(Message::DismissUpdate).style(style::link_button(t.text.0, t.header.0)),
                    button(text("Download").size(fs))
                        .padding([8, 16])
                        .on_press(Message::OpenUpdatePage)
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                ]
                .spacing(8)
                .into(),
            ),
            _ => return None,
        };
        Some(
            container(
                column![
                    row![
                        container(text("↑").size(fs * 1.4).font(BOLD).color(contrast_text(t.accent.0)))
                            .center(40)
                            .style(card(t.accent.0, 20.0)),
                        text(title).size(fs * 1.3).font(BOLD).color(t.header.0),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                    body,
                    buttons,
                ]
                .spacing(16),
            )
            .width(460)
            .padding(24)
            .style(card(t.chat.0, t.radius * 1.5))
            .into(),
        )
    }

    fn updates_section(&self) -> Element<'_, Message> {
        use crate::updater::{Stage, VERSION};
        let t = &self.theme;
        let fs = self.fs();
        let status = match &self.update {
            Stage::Checking => "Checking for updates...".to_string(),
            Stage::UpToDate => "You have the newest version.".into(),
            Stage::Ready { version, .. } => format!("{version} is ready to install."),
            Stage::Postponed { version } => format!("{version} will install the next time you open SynCord."),
            Stage::Manual { version, .. } => format!("{version} is available to download."),
            Stage::Installing(v) => format!("Installing {v}..."),
            Stage::Failed(e) => format!("Couldn't check for updates: {e}"),
        };
        let busy = matches!(self.update, Stage::Checking | Stage::Installing(_));
        let mut col = column![
            label(t, "Updates", fs),
            text(format!("SynCord {VERSION}")).size(fs).color(t.header.0),
            text(status).size(fs * 0.85).color(t.muted.0),
            button(text("Check for updates").size(fs * 0.9))
                .padding([6, 14])
                .on_press_maybe((!busy).then_some(Message::CheckForUpdates))
                .style(style::solid_button(t.floating.0, t.radius * 0.6)),
        ]
        .spacing(8);
        if let Some(s) = &self.session {
            let server =
                if s.server_version.is_empty() { "older than 0.3".to_string() } else { s.server_version.clone() };
            col = col.push(text(format!("Server version: {server}")).size(fs * 0.85).color(t.muted.0));
            let (security, color) = match &self.encrypted {
                Some(Some(print)) => (
                    format!("Connection: encrypted (server fingerprint {})", &print[..23.min(print.len())]),
                    t.online.0,
                ),
                Some(None) => {
                    ("Connection: not encrypted. Updating the server turns encryption on.".to_string(), t.danger.0)
                }
                None => (String::new(), t.muted.0),
            };
            if !security.is_empty() {
                col = col.push(text(security).size(fs * 0.85).color(color));
            }
            if s.is_owner()
                && syncord_update::is_newer(
                    VERSION,
                    if s.server_version.is_empty() { "0.0.0" } else { &s.server_version },
                )
            {
                col = col.push(
                    text("Your server is older than this app. Windows servers update themselves overnight; on other systems, install the new server package.")
                        .size(fs * 0.85)
                        .color(rgb(0xF0B232)),
                );
            }
        }
        col.into()
    }

    fn appearance_settings(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();

        let tiles = self
            .themes
            .iter()
            .fold(row![].spacing(12), |r, theme| r.push(theme_tile(t, theme, theme.name == self.theme.name, fs)));

        let mut col = column![
            text("Appearance").size(fs * 1.4).font(BOLD).color(t.header.0),
            label(t, "Theme", fs),
            tiles.wrap().vertical_spacing(12),
            label(t, "Custom themes", fs),
            text(match themes_dir() {
                Some(dir) => format!(
                    "Drop .toml or .css theme files into {}. An example.toml is there to copy. Then press Reload.",
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
        col = col.push(label(t, "Theme editor", fs)).push(self.theme_editor_view());

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
            )
            .push(
                toggler(self.settings.autoplay_gifs)
                    .label("Play GIFs automatically (otherwise when you point at them)")
                    .on_toggle(Message::ToggleAutoplayGifs)
                    .text_size(fs),
            )
            .push(label(t, "Performance", fs))
            .push(
                toggler(self.settings.low_memory)
                    .label("Low memory mode (draws on the CPU; uses far less RAM)")
                    .on_toggle(Message::ToggleLowMemory)
                    .text_size(fs),
            )
            .push(text("Takes effect the next time you open SynCord.").size(fs * 0.8).color(t.muted.0))
            .push(label(t, "Closing the window", fs))
            .push(if crate::tray::SUPPORTED {
                Element::from(
                    toggler(self.settings.close_to_tray)
                        .label("Keep SynCord running in the tray when you close it (calls and messages keep working)")
                        .on_toggle(Message::ToggleCloseToTray)
                        .text_size(fs),
                )
            } else {
                text("On Linux, closing the window quits SynCord.").size(fs * 0.85).color(t.muted.0).into()
            })
            .push(self.updates_section());
        col.into()
    }
}

/// A clickable miniature of a theme's layout.
fn theme_tile<'a>(current: &AppTheme, theme: &'a AppTheme, selected: bool, fs: f32) -> Element<'a, Message> {
    let block = |c: Color, w: f32, h: f32, r: f32| container(space()).width(w).height(h).style(style::flat(c, r));
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

    let border_color = if selected { current.accent.0 } else { Color::TRANSPARENT };
    let radius = current.radius;
    button(
        column![
            container(container(mini).clip(true).style(style::flat(theme.chat.0, radius * 0.6))).padding(3).style(
                move |_| container::Style {
                    border: Border { radius: (radius + 2.0).into(), width: 3.0, color: border_color },
                    ..Default::default()
                }
            ),
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

/// Shorten to `max` characters with an ellipsis so sidebars stay on one line.
/// The red count of unread pings.
pub(crate) fn short_time_pub(ms: i64) -> String {
    short_time(ms)
}

pub(crate) fn mention_badge<'a>(t: &AppTheme, count: u32, fs: f32) -> Element<'a, Message> {
    let danger = t.danger.0;
    container(text(count.min(99).to_string()).size(fs * 0.7).font(BOLD).color(Color::WHITE))
        .padding([1, 6])
        .style(move |_| container::Style {
            background: Some(danger.into()),
            border: Border { radius: 9.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn local(ms: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

fn short_time(ms: i64) -> String {
    local(ms).map(|d| d.format("%-I:%M %p").to_string()).unwrap_or_default()
}

pub(crate) fn long_time(ms: i64) -> String {
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

/// Horizontal bar showing the mic level, with a marker at the sensitivity threshold.
fn level_meter<'a>(t: &AppTheme, level_db: f32, threshold_db: f32, live: bool) -> Element<'a, Message> {
    const WIDTH: f32 = 360.0;
    const MIN_DB: f32 = -80.0;
    const MAX_DB: f32 = -10.0;
    let frac = |db: f32| ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0);
    let level = if live { frac(level_db) } else { 0.0 };
    let fill_color = if live && level_db >= threshold_db { t.online.0 } else { rgb(0xF0B232) };
    let track = t.rail.0;
    let header = t.header.0;

    let fill = container(space()).width(WIDTH * level).height(10).style(move |_| container::Style {
        background: Some(fill_color.into()),
        border: Border { radius: 5.0.into(), ..Default::default() },
        ..Default::default()
    });
    let marker = container(space().width(3).height(18)).style(move |_| container::Style {
        background: Some(header.into()),
        border: Border { radius: 1.5.into(), ..Default::default() },
        ..Default::default()
    });
    let bar = container(fill).width(WIDTH).height(10).style(move |_| container::Style {
        background: Some(track.into()),
        border: Border { radius: 5.0.into(), ..Default::default() },
        ..Default::default()
    });
    stack![
        container(bar).height(18).align_y(Alignment::Center),
        container(marker).padding(Padding { left: (WIDTH * frac(threshold_db) - 1.5).max(0.0), ..Padding::ZERO }),
    ]
    .width(WIDTH)
    .into()
}
