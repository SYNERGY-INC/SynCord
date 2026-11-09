//! Keyboard shortcuts (the same keys Discord uses), the quick switcher (Ctrl+K)
//! and the list of shortcuts (Ctrl+/).

use celerity_proto::{ChannelId, ChannelKind, UserId};
use iced::keyboard::{Key, Modifiers, key};
use iced::widget::{Id, button, column, container, operation, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length, Task};

use crate::app::{App, Message, Overlay, PickerTarget, Session};
use crate::style::{self, bold, card, label, semibold};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    QuickSwitcher,
    Search,
    /// Previous (-1) or next (1) channel.
    Channel(i32),
    /// Previous or next channel with unread messages.
    Unread(i32),
    ToggleMute,
    ToggleDeafen,
    Emoji,
    Gif,
    Upload,
    Settings,
    Help,
    /// Page up (-1) or down (1) in the messages.
    Page(i32),
    /// Arrow keys in the quick switcher.
    SwitcherMove(i32),
}

/// Shortcuts, their keys and what they do, for the Ctrl+/ list.
pub const LIST: &[(&str, &str)] = &[
    ("Ctrl + K", "Jump to a channel, conversation or person"),
    ("Ctrl + F", "Search messages"),
    ("Alt + Up / Down", "Previous or next channel"),
    ("Alt + Shift + Up / Down", "Previous or next unread channel"),
    ("Ctrl + Shift + M", "Mute or unmute your microphone"),
    ("Ctrl + Shift + D", "Deafen or undeafen"),
    ("Ctrl + E", "Emoji picker"),
    ("Ctrl + G", "GIF picker"),
    ("Ctrl + Shift + U", "Upload a file"),
    ("Ctrl + V", "Paste a screenshot or copied files"),
    ("Ctrl + C", "Copy the picture you're viewing"),
    ("Ctrl + ,", "Settings"),
    ("Ctrl + /", "This list"),
    ("Page Up / Page Down", "Scroll through messages"),
    ("Enter / Shift + Enter", "Send / new line"),
    ("Up (empty message box)", "Edit your last message"),
    ("Tab", "Pick an emoji or @mention suggestion"),
    ("Escape", "Close whatever is open"),
];

/// Which shortcut a key press is, if any. `switcher`: the quick switcher is open.
pub fn from_key(key: &Key, physical: key::Physical, m: Modifiers, switcher: bool) -> Option<Shortcut> {
    use key::Named;
    let letter = |c: char, code: key::Code| {
        matches!(key, Key::Character(s) if s.eq_ignore_ascii_case(&c.to_string()))
            || physical == key::Physical::Code(code)
    };
    if switcher && !m.command() && !m.alt() {
        match key {
            Key::Named(Named::ArrowUp) => return Some(Shortcut::SwitcherMove(-1)),
            Key::Named(Named::ArrowDown) => return Some(Shortcut::SwitcherMove(1)),
            _ => {}
        }
    }
    if m.alt() && !m.command() {
        let step = match key {
            Key::Named(Named::ArrowUp) => -1,
            Key::Named(Named::ArrowDown) => 1,
            _ => return None,
        };
        return Some(if m.shift() { Shortcut::Unread(step) } else { Shortcut::Channel(step) });
    }
    if !m.command() && !m.alt() {
        return match key {
            Key::Named(Named::PageUp) => Some(Shortcut::Page(-1)),
            Key::Named(Named::PageDown) => Some(Shortcut::Page(1)),
            _ => None,
        };
    }
    if !m.command() || m.alt() {
        return None;
    }
    if m.shift() {
        return if letter('m', key::Code::KeyM) {
            Some(Shortcut::ToggleMute)
        } else if letter('d', key::Code::KeyD) {
            Some(Shortcut::ToggleDeafen)
        } else if letter('u', key::Code::KeyU) {
            Some(Shortcut::Upload)
        } else {
            None
        };
    }
    if letter('k', key::Code::KeyK) {
        Some(Shortcut::QuickSwitcher)
    } else if letter('f', key::Code::KeyF) {
        Some(Shortcut::Search)
    } else if letter('e', key::Code::KeyE) {
        Some(Shortcut::Emoji)
    } else if letter('g', key::Code::KeyG) {
        Some(Shortcut::Gif)
    } else if matches!(key, Key::Character(s) if s.as_str() == ",") || physical == key::Physical::Code(key::Code::Comma)
    {
        Some(Shortcut::Settings)
    } else if matches!(key, Key::Character(s) if s.as_str() == "/") || physical == key::Physical::Code(key::Code::Slash)
    {
        Some(Shortcut::Help)
    } else {
        None
    }
}

/// A place the quick switcher can jump to.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Channel(ChannelId),
    Person(UserId),
}

/// Channels you can switch between with Alt+Up/Down, in the order the sidebar shows them.
fn channel_order(s: &Session) -> Vec<ChannelId> {
    if s.view == crate::app::View::Home {
        s.directs.iter().map(|d| d.id).collect()
    } else {
        s.channels.iter().filter(|c| c.kind == ChannelKind::Text).map(|c| c.id).collect()
    }
}

fn switcher_targets(s: &Session, query: &str) -> Vec<(Target, String, &'static str)> {
    let q = query.trim().trim_start_matches(['#', '@']).to_lowercase();
    let mut out = Vec::new();
    for c in s.channels.iter().filter(|c| c.kind == ChannelKind::Text) {
        if c.name.to_lowercase().contains(&q) {
            out.push((Target::Channel(c.id), format!("# {}", c.name), "Channel"));
        }
    }
    let mut people: Vec<_> = s.users.values().filter(|u| u.id != s.me).collect();
    people.sort_by_key(|u| u.name().to_lowercase());
    for u in people {
        if u.name().to_lowercase().contains(&q) || u.username.to_lowercase().contains(&q) {
            out.push((Target::Person(u.id), format!("@ {}", u.name()), "Message"));
        }
    }
    out.truncate(12);
    out
}

impl App {
    pub(crate) fn on_shortcut(&mut self, shortcut: Shortcut) -> Task<Message> {
        let Some(s) = &self.session else { return Task::none() };
        match shortcut {
            Shortcut::QuickSwitcher => {
                if matches!(self.overlay, Some(Overlay::Switcher { .. })) {
                    self.close_overlay();
                    return Task::none();
                }
                self.close_overlay();
                self.overlay = Some(Overlay::Switcher { query: String::new(), index: 0 });
                return operation::focus(Id::new("switcher"));
            }
            Shortcut::Channel(step) | Shortcut::Unread(step) => {
                let order = channel_order(s);
                if order.is_empty() {
                    return Task::none();
                }
                let unread_only = matches!(shortcut, Shortcut::Unread(_));
                let here = s.current.and_then(|c| order.iter().position(|x| *x == c)).unwrap_or(0) as i32;
                let n = order.len() as i32;
                for k in 1..=n {
                    let id = order[(here + step * k).rem_euclid(n) as usize];
                    if !unread_only || s.is_unread(id) {
                        return self.select_channel(id);
                    }
                }
            }
            Shortcut::Search => {
                self.close_overlay();
                return self.update(Message::OpenSearch);
            }
            Shortcut::ToggleMute => return self.update(Message::ToggleMute),
            Shortcut::ToggleDeafen => return self.update(Message::ToggleDeafen),
            Shortcut::Emoji => return self.update(Message::OpenEmojiPicker(PickerTarget::Composer)),
            Shortcut::Gif => return self.update(Message::OpenGifPicker),
            Shortcut::Upload => return self.update(Message::AttachFiles),
            Shortcut::Settings => return self.update(Message::OpenSettings),
            Shortcut::Help => {
                self.close_overlay();
                self.overlay = Some(Overlay::Shortcuts);
            }
            Shortcut::Page(step) => {
                // The list is anchored to the bottom, so "down" is towards the newest.
                let amount = 400.0 * -step as f32;
                return operation::scroll_by(
                    Id::new(crate::app::MESSAGES_ID),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: amount },
                );
            }
            Shortcut::SwitcherMove(step) => {
                let count = match &self.overlay {
                    Some(Overlay::Switcher { query, .. }) => switcher_targets(s, query).len() as i32,
                    _ => 0,
                };
                if let Some(Overlay::Switcher { index, .. }) = &mut self.overlay
                    && count > 0
                {
                    *index = (*index as i32 + step).rem_euclid(count) as usize;
                }
            }
        }
        Task::none()
    }

    pub(crate) fn update_switcher(&mut self, message: Message) -> Task<Message> {
        let Some(s) = &self.session else { return Task::none() };
        match message {
            Message::SwitcherQuery(q) => {
                if let Some(Overlay::Switcher { query, index }) = &mut self.overlay {
                    *query = q;
                    *index = 0;
                }
            }
            Message::SwitcherGo(i) => {
                let Some(Overlay::Switcher { query, index }) = &self.overlay else { return Task::none() };
                let targets = switcher_targets(s, query);
                let pick = targets.get(if i == usize::MAX { *index } else { i }).map(|(t, _, _)| *t);
                self.close_overlay();
                return match pick {
                    Some(Target::Channel(id)) => self.select_channel(id),
                    Some(Target::Person(id)) => self.update(Message::OpenDirect(id)),
                    None => Task::none(),
                };
            }
            _ => {}
        }
        Task::none()
    }

    pub(crate) fn switcher_dialog<'a>(&'a self, s: &'a Session, query: &'a str, index: usize) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut list = column![].spacing(2);
        let targets = switcher_targets(s, query);
        for (i, (_, name, kind)) in targets.iter().enumerate() {
            list = list.push(
                button(
                    row![
                        text(name.clone()).size(fs).color(t.header.0).width(Length::Fill),
                        text(*kind).size(fs * 0.75).color(t.muted.0)
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([7, 10])
                .on_press(Message::SwitcherGo(i))
                .style(style::row_button(t, i == index.min(targets.len().saturating_sub(1)))),
            );
        }
        if targets.is_empty() {
            list = list.push(text("Nothing matches.").size(fs * 0.9).color(t.muted.0));
        }
        let body = column![
            text_input("Where would you like to go?", query)
                .id(Id::new("switcher"))
                .on_input(Message::SwitcherQuery)
                .on_submit(Message::SwitcherGo(usize::MAX))
                .padding(12)
                .size(fs * 1.1)
                .style(style::input(t, t.rail.0)),
            scrollable(list).height(Length::Shrink).style(style::scroll(t)),
            text("Up and Down to choose, Enter to go").size(fs * 0.75).color(t.muted.0),
        ]
        .spacing(12);
        container(body).width(520).padding(18).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    pub(crate) fn shortcuts_dialog(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut list = column![text("Keyboard shortcuts").size(fs * 1.3).font(bold()).color(t.header.0)].spacing(8);
        for (keys, what) in LIST {
            list = list.push(
                row![
                    container(text(*keys).size(fs * 0.85).font(semibold()).color(t.header.0))
                        .padding([3, 8])
                        .width(220)
                        .style(card(t.floating.0, 6.0)),
                    text(*what).size(fs * 0.9).color(t.text.0),
                ]
                .spacing(14)
                .align_y(Alignment::Center),
            );
        }
        list = list.push(label(t, "On a Mac, use Cmd instead of Ctrl", fs));
        container(
            scrollable(container(list).padding(iced::Padding { right: 18.0, ..iced::Padding::ZERO }))
                .style(style::scroll(t)),
        )
        .width(620)
        .max_height(560)
        .padding(22)
        .style(card(t.chat.0, t.radius * 1.5))
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let ctrl = Modifiers::CTRL;
        let k = |c: &str| Key::Character(c.into());
        let none = key::Physical::Unidentified(key::NativeCode::Unidentified);
        assert_eq!(from_key(&k("k"), none, ctrl, false), Some(Shortcut::QuickSwitcher));
        assert_eq!(from_key(&k("M"), none, ctrl | Modifiers::SHIFT, false), Some(Shortcut::ToggleMute));
        assert_eq!(
            from_key(&Key::Named(key::Named::ArrowDown), none, Modifiers::ALT, false),
            Some(Shortcut::Channel(1))
        );
        assert_eq!(
            from_key(&Key::Named(key::Named::ArrowUp), none, Modifiers::ALT | Modifiers::SHIFT, false),
            Some(Shortcut::Unread(-1))
        );
        assert_eq!(from_key(&k("k"), none, Modifiers::empty(), false), None);
        // Russian layout: the K key types "л", but its position still counts.
        assert_eq!(from_key(&k("л"), key::Physical::Code(key::Code::KeyK), ctrl, false), Some(Shortcut::QuickSwitcher));
        assert_eq!(
            from_key(&Key::Named(key::Named::ArrowDown), none, Modifiers::empty(), true),
            Some(Shortcut::SwitcherMove(1))
        );
    }
}
