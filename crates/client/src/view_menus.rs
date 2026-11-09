//! Right-click menus.

use celerity_proto::{User, UserId};
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Border, Color, Element, Length};

use crate::app::{App, Message};
use crate::style::{self, semibold};
use crate::theme::rgb;
use crate::widgets::context_menu;

impl App {
    /// `base`, with the menu for person `user` on right-click.
    pub(crate) fn with_user_menu<'a>(
        &'a self,
        base: impl Into<Element<'a, Message>>,
        user: UserId,
    ) -> Element<'a, Message> {
        context_menu(base, move || self.user_menu(user)).into()
    }

    /// Right-click in the message box.
    pub(crate) fn composer_menu(&self) -> Element<'_, Message> {
        use crate::composer::MenuAction::*;
        let t = &self.theme;
        let fs = self.fs();
        let selected = self.editor.selection().is_some_and(|s| !s.is_empty());
        let item = |label: &'static str, keys: &'static str, action, on: bool| {
            let color = if on { t.text.0 } else { t.muted.0 };
            button(
                row![
                    text(label).size(fs * 0.9).color(color).width(Length::Fill),
                    text(keys).size(fs * 0.75).color(t.muted.0)
                ]
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([6, 10])
            .on_press_maybe(on.then_some(Message::ComposerMenu(action)))
            .style(style::row_button(t, false))
        };
        let sep = || container(style::divider(t.hover.0)).padding([4, 6]);
        let col = column![
            item("Undo", "Ctrl+Z", Undo, self.history.can_undo()),
            item("Redo", "Ctrl+Y", Redo, self.history.can_redo()),
            sep(),
            item("Cut", "Ctrl+X", Cut, selected),
            item("Copy", "Ctrl+C", Copy, selected),
            item("Paste", "Ctrl+V", Paste, true),
            sep(),
            item("Select All", "Ctrl+A", SelectAll, !self.composer.is_empty()),
        ]
        .spacing(1);
        container(col)
            .padding(6)
            .width(220)
            .style(move |_| container::Style {
                background: Some(style::darken(t.floating.0, 0.12).into()),
                border: Border { radius: (t.radius * 0.8).into(), width: 1.0, color: t.hover.0 },
                ..Default::default()
            })
            .into()
    }

    /// A short menu of actions.
    pub(crate) fn simple_menu(&self, items: Vec<(String, Message)>) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut col = column![].spacing(1);
        for (label, msg) in items {
            col = col.push(
                button(text(label).size(fs * 0.9).color(t.text.0))
                    .width(Length::Fill)
                    .padding([6, 10])
                    .on_press(msg)
                    .style(style::row_button(t, false)),
            );
        }
        container(col)
            .padding(6)
            .width(200)
            .style(move |_| container::Style {
                background: Some(style::darken(t.floating.0, 0.12).into()),
                border: Border { radius: (t.radius * 0.8).into(), width: 1.0, color: t.hover.0 },
                ..Default::default()
            })
            .into()
    }

    /// `base` with a channel's (or DM's) right-click menu: mark read, mute.
    pub(crate) fn with_channel_menu<'a>(
        &'a self,
        base: impl Into<Element<'a, Message>>,
        channel: celerity_proto::ChannelId,
        direct: bool,
    ) -> Element<'a, Message> {
        context_menu(base, move || {
            let muted = self.settings.channel_muted(&self.settings.server, channel);
            let what = if direct { "conversation" } else { "channel" };
            let mut items = vec![
                ("Mark as read".into(), Message::MarkChannelRead(channel)),
                (
                    if muted { format!("Unmute {what}") } else { format!("Mute {what}") },
                    Message::ToggleMuteChannel(channel),
                ),
            ];
            let manage = self.session.as_ref().filter(|s| !direct && s.can(celerity_proto::perms::MANAGE_CHANNELS));
            if let Some(s) = manage {
                if s.v07 {
                    let edit =
                        crate::app_channels::ChannelsMsg::Edit(crate::app_channels::EditTarget::Channel(channel));
                    items.push(("Edit channel".into(), Message::Channels(edit)));
                }
                if s.channels.len() > 1 {
                    items.push(("Delete channel".into(), Message::AskDeleteChannel(channel)));
                }
            }
            self.simple_menu(items)
        })
        .into()
    }

    /// `base` with a server tile's right-click menu.
    pub(crate) fn with_server_menu<'a>(
        &'a self,
        base: impl Into<Element<'a, Message>>,
        position: usize,
        key: String,
    ) -> Element<'a, Message> {
        context_menu(base, move || {
            let muted = self.settings.muted_servers.contains(&key);
            let mut items = Vec::new();
            // Invites are made on the open server.
            let open = position == self.settings.server_position.min(self.parked.len());
            if open && self.session.as_ref().is_some_and(|s| s.v07 && s.can(celerity_proto::perms::CREATE_INVITES)) {
                items.push(("Invite people".into(), Message::Invites(crate::app_invites::InviteMsg::Open)));
            }
            items.push(("Mark as read".into(), Message::MarkServerRead(position)));
            items.push((
                if muted { "Unmute server" } else { "Mute server" }.into(),
                Message::ToggleMuteServer(position),
            ));
            self.simple_menu(items)
        })
        .into()
    }

    fn user_menu(&self, user: UserId) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(s) = &self.session else { return column![].into() };
        let Some(u): Option<&User> = s.users.get(&user) else { return column![].into() };
        let me = user == s.me;
        let item = |label: String, msg: Message, color: Color| {
            button(text(label).size(fs * 0.9).color(color))
                .width(Length::Fill)
                .padding([6, 10])
                .on_press(msg)
                .style(style::row_button(t, false))
        };
        let sep = || container(style::divider(t.hover.0)).padding([4, 6]);
        let mut col = column![item("Profile".into(), Message::ShowProfile(user), t.text.0)].spacing(1);
        if !me {
            col = col.push(item("Message".into(), Message::OpenDirect(user), t.text.0));
            col = col.push(item("Start a Call".into(), Message::StartCall(user), t.text.0));
        }

        // Roles you can hand out.
        let manageable: Vec<_> = s.roles.iter().filter(|r| s.can_manage_role(r)).collect();
        if !manageable.is_empty() && !(u.is_owner && !s.is_owner()) {
            col = col.push(sep());
            col = col.push(container(style::label(t, "Roles", fs)).padding([4, 10]));
            for r in manageable {
                let has = u.roles.contains(&r.id);
                let dot = r.color.map(rgb).unwrap_or(t.muted.0);
                let line = row![
                    container(text(if has { "✓" } else { "" }).size(fs * 0.85).color(t.header.0)).width(16),
                    container(iced::widget::space()).width(10).height(10).style(move |_| container::Style {
                        background: Some(dot.into()),
                        border: Border { radius: 5.0.into(), ..Default::default() },
                        ..Default::default()
                    }),
                    text(r.name.clone()).size(fs * 0.9).color(t.text.0),
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                col = col.push(
                    button(line)
                        .width(Length::Fill)
                        .padding([5, 10])
                        .on_press(Message::ToggleUserRole(user, r.id))
                        .style(style::row_button(t, false)),
                );
            }
        }

        if !me {
            col = col.push(sep());
            let ignored = self.ignores(user);
            col = col.push(item(
                if ignored { "Stop Ignoring" } else { "Ignore" }.into(),
                Message::ToggleIgnore(user),
                t.text.0,
            ));
            let blocked = s.blocked.contains(&user);
            col = col.push(item(
                if blocked { "Unblock" } else { "Block" }.into(),
                Message::ToggleBlock(user),
                t.danger.0,
            ));
            let moderation = self.moderation_items(s, u);
            if !moderation.is_empty() {
                col = col.push(sep());
                for (label, msg) in moderation {
                    col = col.push(item(label, msg, t.danger.0));
                }
            }
        }
        col = col.push(sep());
        col = col.push(item("Copy Username".into(), Message::CopyText(u.username.clone()), t.text.0));
        col = col.push(item("Copy User ID".into(), Message::CopyText(u.id.to_string()), t.text.0));

        let header = text(u.name().to_owned()).size(fs * 0.8).font(semibold()).color(t.muted.0);
        container(column![container(header).padding([4, 10]), col].spacing(2))
            .padding(6)
            .width(220)
            .style(move |_| container::Style {
                background: Some(style::darken(t.floating.0, 0.12).into()),
                border: Border { radius: (t.radius * 0.8).into(), width: 1.0, color: t.hover.0 },
                ..Default::default()
            })
            .into()
    }
}
