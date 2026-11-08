//! Right-click menu on messages, copying message text, and selecting part of a message.

use std::sync::atomic::{AtomicU64, Ordering};

use iced::widget::{Id, button, column, container, mouse_area, operation, pin, responsive, stack, text, text_editor};
use iced::{Element, Length, Point, Task};
use syncord_proto::{Message as ChatMessage, MessageId};

use crate::app::{App, Message, PickerTarget};
use crate::style::{self, card};

/// Width of the message menu, used to keep it on screen.
const MENU_WIDTH: f32 = 200.0;
/// Rough height of the message menu (five rows), used to keep it on screen.
const MENU_HEIGHT: f32 = 225.0;

/// Where the mouse pointer last was, in window coordinates (two `f32`s packed together).
/// Kept outside the app state so moving the mouse doesn't redraw the window.
static CURSOR: AtomicU64 = AtomicU64::new(0);

/// Remember where the pointer is. Called from the event subscription for every mouse move.
pub fn remember_cursor(p: Point) {
    CURSOR.store((p.x.to_bits() as u64) << 32 | p.y.to_bits() as u64, Ordering::Relaxed);
}

fn last_cursor() -> Point {
    let v = CURSOR.load(Ordering::Relaxed);
    Point::new(f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32))
}

/// Text being selected in a message (shown in a read-only text box).
pub struct Selecting {
    pub message_id: MessageId,
    pub content: text_editor::Content,
}

impl App {
    /// The message with this id in the channel being shown.
    pub(crate) fn find_message(&self, id: MessageId) -> Option<&ChatMessage> {
        let s = self.session.as_ref()?;
        s.chans.get(&s.current?)?.messages.iter().find(|m| m.id == id)
    }

    pub(crate) fn update_messages(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenMessageMenu(id) => {
                if self.editing.as_ref().is_some_and(|(e, _)| *e == id) {
                    return Task::none();
                }
                self.emoji_picker = None;
                self.message_menu = Some((id, last_cursor()));
            }
            Message::CloseMessageMenu => self.message_menu = None,
            Message::CopyMessageText(id) => {
                self.message_menu = None;
                if let Some(m) = self.find_message(id) {
                    return iced::clipboard::write(m.content.clone());
                }
            }
            Message::SelectMessageText(id) => {
                self.message_menu = None;
                // Double-clicks inside the text box (to pick a word) or the edit box land here too.
                let busy = self.selecting.as_ref().is_some_and(|s| s.message_id == id)
                    || self.editing.as_ref().is_some_and(|(e, _)| *e == id);
                if busy {
                    return Task::none();
                }
                let Some(m) = self.find_message(id).filter(|m| !m.content.is_empty()) else { return Task::none() };
                let mut content = text_editor::Content::with_text(&m.content);
                content.perform(text_editor::Action::SelectAll);
                self.selecting = Some(Selecting { message_id: id, content });
                return operation::focus(Id::new(SELECT_ID));
            }
            Message::SelectAction(action) => {
                // Read-only: allow moving, selecting and copying, but not typing.
                if let Some(sel) = &mut self.selecting
                    && !action.is_edit()
                {
                    sel.content.perform(action);
                }
            }
            Message::StopSelecting => self.selecting = None,
            Message::MenuReact(id) => {
                self.message_menu = None;
                return self.update(Message::OpenEmojiPicker(PickerTarget::Reaction(id)));
            }
            Message::StartReply(id) => {
                self.message_menu = None;
                self.selecting = None;
                self.replying = Some(id);
                return operation::focus(Id::new(crate::app::COMPOSER_ID));
            }
            Message::CancelReply => self.replying = None,
            Message::MenuEdit(id) => {
                self.message_menu = None;
                self.selecting = None;
                return self.update(Message::StartEdit(id));
            }
            Message::MenuDelete(id) => {
                self.message_menu = None;
                self.selecting = None;
                return self.update(Message::DeleteMessage(id));
            }
            _ => {}
        }
        Task::none()
    }

    /// Read-only, selectable copy of a message's text.
    pub(crate) fn selectable_body<'a>(&'a self, sel: &'a Selecting, size: f32) -> Element<'a, Message> {
        let t = &self.theme;
        let (text_color, selection, muted) = (t.text.0, crate::theme::with_alpha(t.accent.0, 0.45), t.muted.0);
        column![
            text_editor(&sel.content)
                .id(Id::new(SELECT_ID))
                .on_action(Message::SelectAction)
                .size(size)
                .padding(0)
                .height(Length::Shrink)
                .style(move |_, _| text_editor::Style {
                    background: iced::Color::TRANSPARENT.into(),
                    border: iced::Border::default(),
                    placeholder: muted,
                    value: text_color,
                    selection,
                }),
            row_hint(t, size),
        ]
        .spacing(4)
        .into()
    }

    /// The right-click menu, drawn over everything at the pointer.
    pub(crate) fn with_message_menu<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        let Some((id, at)) = self.message_menu else { return base };
        let Some(s) = &self.session else { return base };
        let Some(m) = self.find_message(id) else { return base };
        let mine = m.author_id == s.me;
        // Roles decide who may tidy up server channels (direct messages: only your own).
        let can_delete =
            mine || (s.can(syncord_proto::perms::MANAGE_MESSAGES) && s.channels.iter().any(|c| c.id == m.channel_id));
        let has_text = !m.content.is_empty();

        // Clicking anywhere else closes the menu.
        let backdrop = mouse_area(container(text("")).width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseMessageMenu)
            .on_right_press(Message::CloseMessageMenu);
        let placed = responsive(move |size| {
            let x = at.x.min(size.width - MENU_WIDTH - 8.0).max(0.0);
            let y = at.y.min(size.height - MENU_HEIGHT - 8.0).max(0.0);
            pin(self.message_menu_items(id, mine, can_delete, has_text)).x(x).y(y).into()
        });
        stack![base, backdrop, placed].into()
    }
}

impl App {
    fn message_menu_items(&self, id: MessageId, mine: bool, can_delete: bool, has_text: bool) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let item = |label: &'static str, msg: Message| {
            button(text(label).size(fs * 0.9))
                .width(Length::Fill)
                .padding([7, 10])
                .on_press(msg)
                .style(style::row_button(t, false))
        };
        let mut items =
            column![item("Add Reaction", Message::MenuReact(id)), item("Reply", Message::StartReply(id))].spacing(2);
        if has_text {
            items = items.push(item("Copy Text", Message::CopyMessageText(id)));
            items = items.push(item("Select Text", Message::SelectMessageText(id)));
        }
        if mine {
            items = items.push(item("Edit Message", Message::MenuEdit(id)));
        }
        if can_delete {
            let danger = t.danger.0;
            items = items.push(
                button(text("Delete Message").size(fs * 0.9))
                    .width(Length::Fill)
                    .padding([7, 10])
                    .on_press(Message::MenuDelete(id))
                    .style(move |_, status| {
                        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                        button::Style {
                            background: hovered.then(|| danger.into()),
                            text_color: if hovered { crate::theme::contrast_text(danger) } else { danger },
                            border: iced::Border { radius: 4.0.into(), ..Default::default() },
                            ..Default::default()
                        }
                    }),
            );
        }
        container(items).width(MENU_WIDTH).padding(6).style(card(t.floating.0, t.radius * 0.6)).into()
    }
}

/// Id of the read-only text box used for selecting message text.
pub const SELECT_ID: &str = "select-message";

fn row_hint<'a>(t: &crate::theme::AppTheme, size: f32) -> Element<'a, Message> {
    let (muted, accent, header) = (t.muted.0, t.accent.0, t.header.0);
    iced::widget::row![
        text("Drag to select, Ctrl+C to copy").size(size * 0.75).color(muted),
        button(text("Done").size(size * 0.75))
            .padding(0)
            .on_press(Message::StopSelecting)
            .style(style::link_button(accent, header)),
    ]
    .spacing(8)
    .into()
}

impl App {
    /// "Replying to <name>" bar above the message box.
    pub(crate) fn reply_bar(&self) -> Option<Element<'_, Message>> {
        let id = self.replying?;
        let s = self.session.as_ref()?;
        let t = &self.theme;
        let fs = self.fs();
        let name =
            self.find_message(id).and_then(|m| s.users.get(&m.author_id)).map(|u| u.name()).unwrap_or("a message");
        let bar = iced::widget::row![
            text("Replying to ").size(fs * 0.85).color(t.muted.0),
            text(name.to_owned()).size(fs * 0.85).font(style::SEMIBOLD).color(t.header.0),
            iced::widget::space::horizontal(),
            button(crate::icons::icon(&crate::icons::CLOSE, fs * 0.9, t.muted.0))
                .padding(4)
                .on_press(Message::CancelReply)
                .style(style::icon_button(t)),
        ]
        .align_y(iced::Alignment::Center);
        Some(container(bar).padding([4, 12]).width(Length::Fill).style(style::card(t.rail.0, t.radius * 0.6)).into())
    }

    /// The line above a reply showing who and what it answers.
    pub(crate) fn reply_preview<'a>(&'a self, r: &'a syncord_proto::ReplyRef) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs() * 0.82;
        let s = self.session.as_ref();
        let mut line = iced::widget::row![crate::icons::icon(&crate::icons::REPLY, fs * 1.1, t.muted.0)]
            .spacing(6)
            .align_y(iced::Alignment::Center);
        match r.author_id.and_then(|a| s.and_then(|s| s.users.get(&a).map(|u| (s, u)))) {
            Some((s, u)) => {
                let snippet: String = r.snippet.lines().next().unwrap_or("").chars().take(90).collect();
                let snippet = if snippet.is_empty() { "(attachment)".to_string() } else { snippet };
                let color =
                    s.role_color(u.id).map(crate::theme::rgb).unwrap_or(crate::theme::rgb(u.profile.accent_color));
                line = line
                    .push(text(u.name().to_owned()).size(fs).font(style::SEMIBOLD).color(color))
                    .push(text(snippet).size(fs).color(t.muted.0));
            }
            None => line = line.push(text("Original message was deleted").size(fs).color(t.muted.0)),
        }
        container(line).padding(iced::Padding { left: 56.0, bottom: 2.0, ..iced::Padding::ZERO }).into()
    }
}
