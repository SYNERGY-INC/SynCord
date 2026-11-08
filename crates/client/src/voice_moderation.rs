//! Moving people between voice channels (drag and drop, or the right-click menu) and
//! server-muting or server-deafening them. Needs the Move members / Mute members permissions.

use iced::widget::{button, column, container, pin, row, stack, text};
use iced::{Alignment, Element, Length, Point, Task};
use syncord_proto::{ChannelId, ChannelKind, ClientMsg, UserId, perms};

use crate::app::{App, Message};
use crate::style::{self, card};

/// How far the pointer must move before a press on someone counts as a drag.
const DRAG_THRESHOLD: f32 = 5.0;

/// Someone in voice being dragged towards another voice channel.
pub struct VoiceDrag {
    pub user_id: UserId,
    pub start: Point,
    pub at: Point,
    /// Voice channel under the pointer.
    pub over: Option<ChannelId>,
}

impl VoiceDrag {
    pub fn moved(&self) -> bool {
        self.start.distance(self.at) > DRAG_THRESHOLD
    }
}

impl App {
    pub(crate) fn update_voice_moderation(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::VoicePress(user_id) => {
                let at = crate::app_messages::last_cursor();
                self.voice_drag = Some(VoiceDrag { user_id, start: at, at, over: None });
            }
            Message::VoiceDragMove(at) => {
                if let Some(d) = &mut self.voice_drag {
                    d.at = at;
                }
            }
            Message::VoiceDragOver(over) => {
                if let Some(d) = &mut self.voice_drag {
                    d.over = over;
                }
            }
            Message::VoiceDragRelease => {
                let Some(d) = self.voice_drag.take() else { return Task::none() };
                let can_move = self.session.as_ref().is_some_and(|s| s.can(perms::MOVE_MEMBERS));
                if !d.moved() || !can_move {
                    return self.update(Message::ShowProfile(d.user_id));
                }
                let current = self.session.as_ref().and_then(|s| s.voice.get(&d.user_id)).map(|v| v.channel_id);
                if let Some(target) = d.over
                    && Some(target) != current
                {
                    self.send(ClientMsg::MoveVoice { user_id: d.user_id, channel_id: target });
                }
            }
            Message::VoiceHover(user) => self.voice_hover = user,
            Message::ModerateVoice { user_id, muted, deafened } => {
                self.send(ClientMsg::ModerateVoice { user_id, muted, deafened });
            }
            Message::MoveVoice(user_id, channel_id) => self.send(ClientMsg::MoveVoice { user_id, channel_id }),
            _ => {}
        }
        Task::none()
    }

    /// The name that follows the pointer while dragging someone to another channel.
    pub(crate) fn with_voice_drag<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        let (Some(s), Some(d)) = (&self.session, self.voice_drag.as_ref().filter(|d| d.moved())) else {
            return base;
        };
        if !s.can(perms::MOVE_MEMBERS) {
            return base;
        }
        let Some(u) = s.users.get(&d.user_id) else { return base };
        let t = &self.theme;
        let fs = self.fs();
        let hint = match d.over.and_then(|id| s.channels.iter().find(|c| c.id == id)) {
            Some(c) => format!("Move to {}", c.name),
            None => "Drop on a voice channel".into(),
        };
        let ghost = container(
            row![
                self.avatar(u, 20.0),
                column![
                    text(u.name().to_owned()).size(fs * 0.9).color(t.header.0),
                    text(hint).size(fs * 0.75).color(t.muted.0)
                ]
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([6, 10])
        .style(card(t.floating.0, t.radius * 0.6));
        stack![base, pin(ghost).x(d.at.x + 12.0).y(d.at.y + 8.0)].into()
    }

    /// Right-click menu for someone in a voice channel.
    pub(crate) fn voice_menu(&self, user_id: UserId) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Some(s) = &self.session else { return column![].into() };
        let Some(state) = s.voice.get(&user_id) else { return column![].into() };
        let item = |label: String, msg: Message| {
            button(text(label).size(fs * 0.9).color(t.text.0))
                .width(Length::Fill)
                .padding([6, 10])
                .on_press(msg)
                .style(style::row_button(t, false))
        };
        let check = |on: bool, label: &str| format!("{}{label}", if on { "✓ " } else { "" });
        let (sm, sd) = (state.server_muted, state.server_deafened);
        let mut col = column![item("Profile".into(), Message::ShowProfile(user_id))].spacing(1);
        if s.can(perms::MUTE_MEMBERS) {
            col = col.push(container(style::divider(t.hover.0)).padding([4, 6]));
            col =
                col.push(item(check(sm, "Server Mute"), Message::ModerateVoice { user_id, muted: !sm, deafened: sd }));
            col = col
                .push(item(check(sd, "Server Deafen"), Message::ModerateVoice { user_id, muted: sm, deafened: !sd }));
        }
        let targets: Vec<_> =
            s.channels.iter().filter(|c| c.kind == ChannelKind::Voice && c.id != state.channel_id).collect();
        if s.can(perms::MOVE_MEMBERS) && !targets.is_empty() {
            col = col.push(container(style::divider(t.hover.0)).padding([4, 6]));
            col = col.push(container(style::label(t, "Move to", fs * 0.9)).padding([4, 10]));
            for c in targets {
                col = col.push(item(c.name.clone(), Message::MoveVoice(user_id, c.id)));
            }
        }
        let (bg, border, radius) = (style::darken(t.floating.0, 0.12), t.hover.0, t.radius * 0.8);
        container(col)
            .padding(6)
            .width(210)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: iced::Border { radius: radius.into(), width: 1.0, color: border },
                ..Default::default()
            })
            .into()
    }
}
