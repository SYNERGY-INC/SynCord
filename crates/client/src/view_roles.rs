//! Server settings: roles.

use iced::widget::{button, column, container, row, scrollable, space, text, text_input, toggler};
use iced::{Alignment, Border, Element, Length, Task};
use syncord_proto::{ClientMsg, EVERYONE_ROLE, Role, perms};

use crate::app::{App, Message, RoleDraft, Session};
use crate::style::{self, BOLD, SEMIBOLD, card, label};
use crate::theme::{Hex, rgb, to_u32};

impl App {
    pub(crate) fn update_roles(&mut self, message: Message) -> Task<Message> {
        let Some(s) = &self.session else { return Task::none() };
        match message {
            Message::CreateRole => self.send(ClientMsg::CreateRole { name: "new role".into() }),
            Message::SelectRole(id) => self.role_editing = s.role(id).map(RoleDraft::from_role),
            Message::RoleName(name) => {
                if let Some(d) = &mut self.role_editing {
                    d.name = name.chars().take(32).collect();
                    d.dirty = true;
                }
            }
            Message::RoleColor(c) => {
                if let Some(d) = &mut self.role_editing {
                    d.color = c;
                    d.dirty = true;
                }
            }
            Message::RoleHoist(on) => {
                if let Some(d) = &mut self.role_editing {
                    d.hoist = on;
                    d.dirty = true;
                }
            }
            Message::RolePermission(bit, on) => {
                if let Some(d) = &mut self.role_editing {
                    if on {
                        d.permissions |= bit;
                    } else {
                        d.permissions &= !bit;
                    }
                    d.dirty = true;
                }
            }
            Message::SaveRole => {
                if let Some(d) = &mut self.role_editing
                    && let Some(current) = s.role(d.id)
                {
                    let color = if d.color.trim().is_empty() { None } else { Hex::parse(&d.color).map(to_u32) };
                    let role = Role {
                        name: d.name.clone(),
                        color,
                        permissions: d.permissions,
                        hoist: d.hoist,
                        ..current.clone()
                    };
                    d.dirty = false;
                    self.send(ClientMsg::UpdateRole(role));
                }
            }
            Message::ResetRole => {
                if let Some(d) = &self.role_editing {
                    self.role_editing = s.role(d.id).map(RoleDraft::from_role);
                }
            }
            Message::DeleteRole(id) => self.send(ClientMsg::DeleteRole { role_id: id }),
            Message::MoveRole(id, up) => {
                // The server puts a role at a position and moves the ones from there up,
                // so "up one" is just above the role above it, "down one" is at the one below.
                let list: Vec<&Role> = s.roles.iter().filter(|r| r.id != EVERYONE_ROLE).collect();
                if let Some(i) = list.iter().position(|r| r.id == id) {
                    let target = if up {
                        i.checked_sub(1).map(|j| list[j].position + 1)
                    } else {
                        list.get(i + 1).map(|r| r.position)
                    };
                    if let Some(position) = target {
                        self.send(ClientMsg::UpdateRole(Role { position, ..list[i].clone() }));
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }

    pub(crate) fn roles_settings<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let dot = |color: Option<u32>| {
            let c = color.map(rgb).unwrap_or(t.muted.0);
            container(space()).width(12).height(12).style(move |_| container::Style {
                background: Some(c.into()),
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            })
        };

        let mut list = column![
            text("Roles").size(fs * 1.4).font(BOLD).color(t.header.0),
            text("Roles give people a colour and permissions. Higher roles win, and you can only change roles below your own.")
                .size(fs * 0.85)
                .color(t.muted.0),
            button(text("Create Role").size(fs * 0.9))
                .padding([7, 14])
                .on_press(Message::CreateRole)
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        ]
        .spacing(10);
        let selected = self.role_editing.as_ref().map(|d| d.id);
        for r in &s.roles {
            let count = if r.id == EVERYONE_ROLE {
                s.users.len()
            } else {
                s.users.values().filter(|u| u.roles.contains(&r.id)).count()
            };
            let line = row![
                dot(r.color),
                text(r.name.clone()).size(fs * 0.95).color(t.text.0).width(Length::Fill),
                text(format!("{count}")).size(fs * 0.8).color(t.muted.0),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding([7, 10])
                    .on_press(Message::SelectRole(r.id))
                    .style(style::row_button(t, selected == Some(r.id))),
            );
        }

        let editor: Element<'a, Message> = match &self.role_editing {
            None => container(text("Pick a role to edit it.").size(fs * 0.9).color(t.muted.0)).padding(20).into(),
            Some(d) => self.role_editor(s, d),
        };
        row![container(scrollable(list).style(style::scroll(t))).width(240), editor].spacing(24).into()
    }

    fn role_editor<'a>(&'a self, s: &'a Session, d: &'a RoleDraft) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(role) = s.role(d.id) else { return column![].into() };
        let everyone = d.id == EVERYONE_ROLE;
        let editable = if everyone { s.can(perms::MANAGE_ROLES) } else { s.can_manage_role(role) };
        let mut col = column![].spacing(16).width(Length::Fill);

        if everyone {
            col = col.push(text("@everyone").size(fs * 1.2).font(BOLD).color(t.header.0));
            col = col
                .push(text("What everyone can do, whatever other roles they have.").size(fs * 0.85).color(t.muted.0));
        } else {
            let mut name = text_input("Role name", &d.name).padding(10).size(fs).style(style::input(t, t.rail.0));
            if editable {
                name = name.on_input(Message::RoleName);
            }
            col = col.push(column![label(t, "Role name", fs), name].spacing(8));
            let current = Hex::parse(&d.color).map(to_u32);
            let swatch = row![
                container(space()).width(36).height(36).style(card(current.map(rgb).unwrap_or(t.muted.0), 8.0)),
                text_input("#RRGGBB (empty: no colour)", &d.color)
                    .on_input(Message::RoleColor)
                    .padding(10)
                    .size(fs)
                    .width(220)
                    .style(style::input(t, t.rail.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            col = col.push(
                column![label(t, "Role colour", fs), swatch, style::swatches(current.unwrap_or(0), Message::RoleColor)]
                    .spacing(8),
            );
            col = col.push(
                toggler(d.hoist)
                    .label("Show people with this role separately in the member list")
                    .on_toggle_maybe(editable.then_some(Message::RoleHoist))
                    .text_size(fs * 0.9)
                    .size(fs * 1.3),
            );
            let mut moves = row![label(t, "Order", fs)].spacing(8).align_y(Alignment::Center);
            for (up, name) in [(true, "Move up"), (false, "Move down")] {
                moves = moves.push(
                    button(text(name).size(fs * 0.85))
                        .padding([5, 10])
                        .on_press_maybe(editable.then_some(Message::MoveRole(d.id, up)))
                        .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                );
            }
            col = col.push(moves);
        }

        // Nobody can hand out a permission they don't have themselves.
        let mine = s.permissions_of(s.me);
        let mut list = column![label(t, "Permissions", fs)].spacing(10);
        for (bit, name, about) in perms::LIST {
            let on = d.permissions & bit != 0;
            let allowed = editable && (s.is_owner() || mine & bit != 0);
            list = list.push(
                row![
                    column![
                        text(*name).size(fs * 0.95).font(SEMIBOLD).color(t.header.0),
                        text(*about).size(fs * 0.8).color(t.muted.0)
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    toggler(on)
                        .on_toggle_maybe(allowed.then_some(move |v| Message::RolePermission(*bit, v)))
                        .size(fs * 1.3),
                ]
                .align_y(Alignment::Center),
            );
        }
        col = col.push(list);

        let mut buttons = row![
            button(text("Save Changes").size(fs))
                .padding([8, 16])
                .on_press_maybe((editable && d.dirty).then_some(Message::SaveRole))
                .style(style::solid_button(t.online.0, t.radius * 0.6)),
            button(text("Reset").size(fs))
                .padding([8, 16])
                .on_press_maybe(d.dirty.then_some(Message::ResetRole))
                .style(style::link_button(t.text.0, t.header.0)),
        ]
        .spacing(8);
        if !everyone && editable {
            let danger = t.danger.0;
            buttons = buttons.push(space::horizontal()).push(
                button(text("Delete Role").size(fs))
                    .padding([8, 16])
                    .on_press(Message::DeleteRole(d.id))
                    .style(style::link_button(danger, style::darken(danger, 0.2))),
            );
        }
        col = col.push(buttons);
        if !editable {
            col = col.push(text("This role is above yours, so you can't change it.").size(fs * 0.85).color(t.muted.0));
        }
        col.into()
    }
}
