//! Right-click menus.

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Border, Color, Element, Length};
use syncord_proto::{User, UserId};

use crate::app::{App, Message};
use crate::style::{self, SEMIBOLD};
use crate::theme::rgb;
use crate::widgets::context_menu;

impl App {
    /// `base`, with the menu for person `user` on right-click.
    pub(crate) fn with_user_menu<'a>(&'a self, base: impl Into<Element<'a, Message>>, user: UserId) -> Element<'a, Message> {
        context_menu(base, move || self.user_menu(user)).into()
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
            col = col.push(item(if ignored { "Stop Ignoring" } else { "Ignore" }.into(), Message::ToggleIgnore(user), t.text.0));
            let blocked = s.blocked.contains(&user);
            col = col.push(item(if blocked { "Unblock" } else { "Block" }.into(), Message::ToggleBlock(user), t.danger.0));
        }
        col = col.push(sep());
        col = col.push(item("Copy Username".into(), Message::CopyText(u.username.clone()), t.text.0));
        col = col.push(item("Copy User ID".into(), Message::CopyText(u.id.to_string()), t.text.0));

        let header = text(u.name().to_owned()).size(fs * 0.8).font(SEMIBOLD).color(t.muted.0);
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
