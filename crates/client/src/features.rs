//! Where what 0.7 added plugs into the app: its dialogs, a shared "are you sure?" dialog,
//! and which module handles each new server event.

use celerity_proto::{CategoryId, ChannelId, ClientMsg, EmojiId, ServerMsg, SoundId, StickerId, UserId};
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Task};

use crate::app::{App, Message, Overlay, Session};
use crate::style::{self, bold, card};

/// Dialogs of the 0.7 features (shown like the others, see `Overlay::Dialog`).
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Channel(crate::app_channels::ChannelEdit),
    Confirm(Confirm),
    NewGroup(crate::app_groups::GroupPick),
    GroupSettings(crate::app_groups::GroupEdit),
    Poll(crate::polls::PollDraft),
    Invite,
}

/// "Are you sure?" before something that can't be undone.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub button: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    DeleteCategory(CategoryId),
    DeleteThread(ChannelId),
    Kick(UserId),
    Ban(UserId),
    LeaveGroup(ChannelId),
    DeleteEmoji(EmojiId),
    DeleteSticker(StickerId),
    DeleteSound(SoundId),
}

/// Server events added in 0.7 that one of the feature modules handles.
pub fn is_feature_event(msg: &ServerMsg) -> bool {
    use ServerMsg::*;
    matches!(
        msg,
        InviteCreated(_)
            | Invites { .. }
            | InviteRevoked { .. }
            | MemberRemoved { .. }
            | CategoryUpdated(_)
            | CategoryDeleted { .. }
            | ChannelUpdated(_)
            | ThreadCreated(_)
            | ThreadUpdated(_)
            | ThreadDeleted { .. }
            | Threads { .. }
            | GroupUpdated(_)
            | GroupRemoved { .. }
            | EmojiUpdated(_)
            | EmojiDeleted { .. }
            | StickerUpdated(_)
            | StickerDeleted { .. }
            | SoundUpdated(_)
            | SoundDeleted { .. }
            | SoundPlayed { .. }
            | PollUpdated { .. }
    )
}

impl App {
    pub(crate) fn on_feature_event(&mut self, msg: ServerMsg) -> Task<Message> {
        use ServerMsg::*;
        match msg {
            InviteCreated(_) | Invites { .. } | InviteRevoked { .. } | MemberRemoved { .. } => {
                self.on_invite_event(msg)
            }
            CategoryUpdated(_) | CategoryDeleted { .. } | ChannelUpdated(_) => self.on_channel_event(msg),
            ThreadCreated(_) | ThreadUpdated(_) | ThreadDeleted { .. } | Threads { .. } => self.on_thread_event(msg),
            GroupUpdated(_) | GroupRemoved { .. } => self.on_group_event(msg),
            SoundPlayed { .. } => self.on_sound_played(msg),
            PollUpdated { .. } => self.on_poll_event(msg),
            _ => self.on_expression_event(msg),
        }
    }

    /// Ask before doing `action`.
    pub(crate) fn ask(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        button: impl Into<String>,
        action: ConfirmAction,
    ) {
        self.close_overlay();
        self.overlay = Some(Overlay::Dialog(Dialog::Confirm(Confirm {
            title: title.into(),
            body: body.into(),
            button: button.into(),
            action,
        })));
    }

    /// The person said yes.
    pub(crate) fn confirmed(&mut self, action: ConfirmAction) -> Task<Message> {
        self.close_overlay();
        let msg = match action {
            ConfirmAction::DeleteCategory(category_id) => ClientMsg::DeleteCategory { category_id },
            ConfirmAction::DeleteThread(thread_id) => ClientMsg::DeleteThread { thread_id },
            ConfirmAction::Kick(user_id) => ClientMsg::KickMember { user_id },
            ConfirmAction::Ban(user_id) => ClientMsg::BanMember { user_id },
            ConfirmAction::LeaveGroup(channel_id) => ClientMsg::LeaveGroup { channel_id },
            ConfirmAction::DeleteEmoji(emoji_id) => ClientMsg::DeleteEmoji { emoji_id },
            ConfirmAction::DeleteSticker(sticker_id) => ClientMsg::DeleteSticker { sticker_id },
            ConfirmAction::DeleteSound(sound_id) => ClientMsg::DeleteSound { sound_id },
        };
        self.send(msg);
        Task::none()
    }

    pub(crate) fn dialog_view<'a>(&'a self, s: &'a Session, d: &'a Dialog) -> Element<'a, Message> {
        match d {
            Dialog::Channel(e) => self.channel_settings_dialog(s, e),
            Dialog::Confirm(c) => self.confirm_dialog(c),
            Dialog::NewGroup(p) => self.new_group_dialog(s, p),
            Dialog::GroupSettings(g) => self.group_settings_dialog(s, g),
            Dialog::Poll(p) => self.poll_dialog(p),
            Dialog::Invite => self.invite_dialog(s),
        }
    }

    fn confirm_dialog<'a>(&'a self, c: &'a Confirm) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let body = column![
            text(c.title.clone()).size(fs * 1.3).font(bold()).color(t.header.0),
            text(c.body.clone()).size(fs).color(t.text.0),
            row![
                space::horizontal(),
                button(text("Cancel").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
                button(text(c.button.clone()).size(fs))
                    .padding([8, 16])
                    .on_press(Message::Confirm(c.action))
                    .style(style::solid_button(t.danger.0, t.radius * 0.6)),
            ]
            .spacing(8),
        ]
        .spacing(14);
        container(body).width(440).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// Escape closes the small menus and popups of the 0.7 features first. True if it did.
    pub(crate) fn close_feature_popups(&mut self) -> bool {
        let open = self.server_menu || self.composer_plus || self.soundboard.open;
        self.server_menu = false;
        self.composer_plus = false;
        self.soundboard.open = false;
        open
    }
}
