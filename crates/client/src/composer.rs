//! The message box: several lines (Shift+Enter), and suggestions for
//! `:emoji:` and `@mentions` while typing.

use iced::widget::text_editor::{self, Action, Binding, Edit, KeyPress, Motion, Status};
use iced::{Task, keyboard};
use syncord_proto::{RoleId, UserId, perms};

use crate::app::{App, Message, Session};
use crate::emoji;

/// Something to finish a half-typed word with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Suggestion {
    Emoji(&'static emojis::Emoji, &'static str),
    User(UserId),
    Role(RoleId),
    Everyone,
}

/// A half-typed `@name` at the end of the text: where it starts and what's typed after the `@`.
pub fn mention_token(text: &str) -> Option<(usize, &str)> {
    let start = text.rfind('@')?;
    let query = &text[start + 1..];
    let preceded_ok = start == 0 || text[..start].ends_with(char::is_whitespace);
    let valid = query.len() <= 32 && !query.contains(char::is_whitespace);
    (preceded_ok && valid).then_some((start, query))
}

/// Most undo steps kept.
const UNDO_LIMIT: usize = 100;
/// Typing faster than this is one undo step (until a space or a pause).
const UNDO_GROUP: std::time::Duration = std::time::Duration::from_millis(1000);

/// Undo and redo for the message box (iced's text box has none).
#[derive(Default)]
pub struct History {
    undo: Vec<String>,
    redo: Vec<String>,
    /// When the current group of typing started being typed.
    last: Option<std::time::Instant>,
}

impl History {
    /// Remember `before` as an undo step, unless it's part of the same burst of typing.
    fn record(&mut self, before: &str, typed: Option<char>) {
        let now = std::time::Instant::now();
        let same_burst = typed.is_some_and(|c| !c.is_whitespace())
            && self.last.is_some_and(|t| now.duration_since(t) < UNDO_GROUP)
            && !self.undo.is_empty();
        if !same_burst {
            self.undo.push(before.to_string());
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.last = typed.map(|_| now);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

/// The right-click menu in the message box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl App {
    /// Replace the message box text (cursor at the end). Emptying it (sending, switching
    /// channels) also forgets the undo history.
    pub(crate) fn set_composer(&mut self, text: String) {
        if text.is_empty() {
            self.history = History::default();
        }
        self.restore_composer(text);
    }

    fn restore_composer(&mut self, text: String) {
        self.editor = text_editor::Content::with_text(&text);
        self.editor.perform(Action::Move(Motion::DocumentEnd));
        self.composer = text;
        self.suggestion_index = 0;
    }

    fn undo(&mut self, redo: bool) -> Task<Message> {
        let (from, to) =
            if redo { (&mut self.history.redo, &mut self.history.undo) } else { (&mut self.history.undo, &mut self.history.redo) };
        let Some(text) = from.pop() else { return Task::none() };
        to.push(self.composer.clone());
        self.history.last = None;
        self.restore_composer(text);
        self.typed()
    }

    /// What could finish the word being typed.
    pub(crate) fn composer_suggestions(&self) -> Vec<Suggestion> {
        let Some(s) = &self.session else { return Vec::new() };
        if let Some((_, query)) = mention_token(&self.composer) {
            return mention_suggestions(s, query, 8);
        }
        match emoji::autocomplete_token(&self.composer) {
            Some((_, code)) => emoji::suggestions(code, 6).into_iter().map(|(e, c)| Suggestion::Emoji(e, c)).collect(),
            None => Vec::new(),
        }
    }

    /// Finish the half-typed word with a suggestion.
    pub(crate) fn accept(&mut self, pick: Suggestion) {
        let Some(s) = &self.session else { return };
        let mut text = self.composer.clone();
        match pick {
            Suggestion::Emoji(e, _) => {
                if let Some((start, _)) = emoji::autocomplete_token(&text) {
                    text.truncate(start);
                    text.push_str(e.as_str());
                    text.push(' ');
                    self.settings.remember_emoji(e.as_str());
                    self.settings.save();
                }
            }
            Suggestion::User(_) | Suggestion::Role(_) | Suggestion::Everyone => {
                let Some((start, _)) = mention_token(&text) else { return };
                let (shown, wire) = match pick {
                    Suggestion::User(id) => {
                        let name = s.users.get(&id).map(|u| u.name().to_string()).unwrap_or_default();
                        (format!("@{name}"), format!("<@{id}>"))
                    }
                    Suggestion::Role(id) => {
                        let name = s.role(id).map(|r| r.name.clone()).unwrap_or_default();
                        (format!("@{name}"), format!("<@&{id}>"))
                    }
                    _ => ("@everyone".to_string(), "@everyone".to_string()),
                };
                text.truncate(start);
                text.push_str(&shown);
                text.push(' ');
                if shown != wire && !self.composer_mentions.iter().any(|(a, _)| *a == shown) {
                    self.composer_mentions.push((shown, wire));
                }
            }
        }
        self.set_composer(text);
    }

    /// The text as sent: chosen mentions become `<@id>`, as do exactly typed `@username`s.
    pub(crate) fn encode_mentions(&self, text: &str, chosen: &[(String, String)]) -> String {
        let Some(s) = &self.session else { return text.to_string() };
        let mut out = text.to_string();
        // Longest first, so "@Sam Smith" wins over "@Sam".
        let mut pairs: Vec<&(String, String)> = chosen.iter().collect();
        pairs.sort_by_key(|(shown, _)| std::cmp::Reverse(shown.len()));
        for (shown, wire) in pairs {
            out = replace_word(&out, shown, wire);
        }
        for u in s.users.values() {
            out = replace_word(&out, &format!("@{}", u.username), &format!("<@{}>", u.id));
        }
        out
    }

    /// Show `<@id>` as `@Name` for editing, remembering how to turn it back.
    pub(crate) fn decode_mentions(&self, text: &str) -> (String, Vec<(String, String)>) {
        let Some(s) = &self.session else { return (text.to_string(), Vec::new()) };
        let (users, roles, _) = syncord_proto::parse_mentions(text);
        let mut out = text.to_string();
        let mut pairs = Vec::new();
        for id in users {
            if let Some(u) = s.users.get(&id) {
                let (wire, shown) = (format!("<@{id}>"), format!("@{}", u.name()));
                out = out.replace(&wire, &shown);
                pairs.push((shown, wire));
            }
        }
        for id in roles {
            if let Some(r) = s.role(id) {
                let (wire, shown) = (format!("<@&{id}>"), format!("@{}", r.name));
                out = out.replace(&wire, &shown);
                pairs.push((shown, wire));
            }
        }
        (out, pairs)
    }

    pub(crate) fn update_composer(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorAction(action) => {
                let edits = action.is_edit();
                let typed = match &action {
                    Action::Edit(Edit::Insert(c)) => Some(*c),
                    _ => None,
                };
                let before = edits.then(|| self.composer.clone());
                self.editor.perform(action);
                if let Some(before) = before {
                    self.composer = self.editor.text().trim_end_matches('\n').to_string();
                    if self.composer != before {
                        self.history.record(&before, typed);
                    }
                    self.suggestion_index = 0;
                    return self.typed();
                }
            }
            Message::Undo => return self.undo(false),
            Message::Redo => return self.undo(true),
            Message::ComposerMenu(action) => {
                let focus = iced::widget::operation::focus(iced::widget::Id::new(crate::app::COMPOSER_ID));
                let then = match action {
                    MenuAction::Undo => self.undo(false),
                    MenuAction::Redo => self.undo(true),
                    MenuAction::Cut | MenuAction::Copy => {
                        if let Some(sel) = self.editor.selection() {
                            let copy = self.update(Message::CopyText(sel));
                            if action == MenuAction::Cut {
                                return Task::batch([copy, self.update_composer(Message::EditorAction(Action::Edit(Edit::Delete))), focus]);
                            }
                            return Task::batch([copy, focus]);
                        }
                        Task::none()
                    }
                    // Like Ctrl+V: pictures and files are attached, text goes in the box.
                    MenuAction::Paste => Task::perform(
                        async { tokio::task::spawn_blocking(crate::app_media::read_clipboard_any).await.ok().flatten() },
                        Message::PastedFromMenu,
                    ),
                    MenuAction::SelectAll => {
                        self.editor.perform(Action::SelectAll);
                        Task::none()
                    }
                };
                return Task::batch([then, focus]);
            }
            Message::PastedFromMenu(clip) => match clip {
                Some(crate::app_media::AnyClip::Text(text)) => {
                    return self
                        .update_composer(Message::EditorAction(Action::Edit(Edit::Paste(std::sync::Arc::new(text)))));
                }
                Some(crate::app_media::AnyClip::Other(clip)) => return self.update(Message::Pasted(Some(clip))),
                None => {}
            },
            Message::SuggestionMove(step) => {
                let n = self.composer_suggestions().len() as i32;
                if n > 0 {
                    self.suggestion_index = (self.suggestion_index as i32 + step).rem_euclid(n) as usize;
                }
            }
            Message::AcceptSuggestion(i) => {
                if let Some(pick) = self.composer_suggestions().get(i).copied() {
                    self.history.record(&self.composer.clone(), None);
                    self.accept(pick);
                }
                return iced::widget::operation::focus(iced::widget::Id::new(crate::app::COMPOSER_ID));
            }
            _ => {}
        }
        Task::none()
    }
}

/// Replace `word` where it stands alone (not part of a longer name).
fn replace_word(text: &str, word: &str, with: &str) -> String {
    if word.len() < 2 || !text.contains(word) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(word) {
        let after = &rest[i + word.len()..];
        let ends_word = after.chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '.'));
        out.push_str(&rest[..i]);
        out.push_str(if ends_word { with } else { word });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn mention_suggestions(s: &Session, query: &str, limit: usize) -> Vec<Suggestion> {
    let q = query.to_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    let in_dm = s.current.is_some_and(|c| s.direct(c).is_some());
    let mut users: Vec<_> = s.users.values().filter(|u| u.id != s.me).collect();
    if in_dm {
        // In a DM, only the other person can be pinged.
        let other = s.current.and_then(|c| s.direct(c)).map(|d| d.user_id);
        users.retain(|u| Some(u.id) == other);
    }
    users.sort_by_key(|u| (!s.online.contains(&u.id), u.name().to_lowercase()));
    for u in users {
        let (name, username) = (u.name().to_lowercase(), u.username.to_lowercase());
        if name.starts_with(&q) || username.starts_with(&q) {
            starts.push(Suggestion::User(u.id));
        } else if name.contains(&q) || username.contains(&q) {
            contains.push(Suggestion::User(u.id));
        }
    }
    if !in_dm {
        for r in s.roles.iter().filter(|r| r.id != syncord_proto::EVERYONE_ROLE) {
            if r.name.to_lowercase().starts_with(&q) {
                starts.push(Suggestion::Role(r.id));
            }
        }
    }
    if (in_dm || s.can(perms::MENTION_EVERYONE)) && "everyone".starts_with(&q) {
        starts.push(Suggestion::Everyone);
    }
    starts.into_iter().chain(contains).take(limit).collect()
}

/// Keys in the message box: Enter sends, Shift+Enter starts a new line, Tab and the
/// arrow keys work the suggestions, and Up in an empty box edits your last message.
pub fn key_binding(kp: KeyPress, suggestions: usize, empty: bool) -> Option<Binding<Message>> {
    use keyboard::key::Named;
    if !matches!(kp.status, Status::Focused { .. }) {
        return None;
    }
    // Ctrl+Z undoes, Ctrl+Y or Ctrl+Shift+Z redoes (Cmd on a Mac).
    if kp.modifiers.command() && !kp.modifiers.alt() {
        match kp.key.to_latin(kp.physical_key).map(|c| c.to_ascii_lowercase()) {
            Some('z') if kp.modifiers.shift() => return Some(Binding::Custom(Message::Redo)),
            Some('z') => return Some(Binding::Custom(Message::Undo)),
            Some('y') => return Some(Binding::Custom(Message::Redo)),
            _ => {}
        }
    }
    match kp.key.as_ref() {
        keyboard::Key::Named(Named::Enter) if kp.modifiers.shift() => Some(Binding::Enter),
        keyboard::Key::Named(Named::Enter) => Some(Binding::Custom(Message::SendMessage)),
        keyboard::Key::Named(Named::Tab) if suggestions > 0 => {
            Some(Binding::Custom(Message::AcceptSuggestion(usize::MAX)))
        }
        keyboard::Key::Named(Named::ArrowUp) if suggestions > 0 => Some(Binding::Custom(Message::SuggestionMove(-1))),
        keyboard::Key::Named(Named::ArrowDown) if suggestions > 0 => Some(Binding::Custom(Message::SuggestionMove(1))),
        keyboard::Key::Named(Named::ArrowUp) if empty => Some(Binding::Custom(Message::EditLast)),
        // Ctrl+Backspace / Ctrl+Delete remove a whole word (Alt on a Mac; Cmd removes to the line start).
        keyboard::Key::Named(Named::Backspace) if kp.modifiers.macos_command() => {
            Some(Binding::Sequence(vec![Binding::Select(Motion::Home), Binding::Backspace]))
        }
        keyboard::Key::Named(Named::Backspace) if kp.modifiers.jump() => {
            Some(Binding::Sequence(vec![Binding::Select(Motion::WordLeft), Binding::Backspace]))
        }
        keyboard::Key::Named(Named::Delete) if kp.modifiers.jump() => {
            Some(Binding::Sequence(vec![Binding::Select(Motion::WordRight), Binding::Delete]))
        }
        _ => Binding::from_key_press(kp),
    }
}

/// Insert text at the cursor (an emoji from the picker).
pub fn insert(editor: &mut text_editor::Content, text: &str) {
    editor.perform(Action::Edit(Edit::Paste(std::sync::Arc::new(text.to_string()))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_groups_typing_into_words() {
        let mut h = History::default();
        let mut text = String::new();
        for c in "hi there".chars() {
            let before = text.clone();
            text.push(c);
            h.record(&before, Some(c));
        }
        // "hi" then " " then "there": undo goes back a word at a time.
        assert_eq!(h.undo, ["", "hi"]);
        h.record(&text, None); // a paste is its own step
        assert_eq!(h.undo.len(), 3);
        assert!(h.can_undo() && !h.can_redo());
    }

    #[test]
    fn undo_keys() {
        let press = |c: &str, m: keyboard::Modifiers| KeyPress {
            key: keyboard::Key::Character(c.into()),
            modified_key: keyboard::Key::Character(c.into()),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            modifiers: m,
            text: None,
            status: Status::Focused { is_hovered: false },
        };
        let ctrl = keyboard::Modifiers::COMMAND;
        assert!(matches!(key_binding(press("z", ctrl), 0, false), Some(Binding::Custom(Message::Undo))));
        let shift = ctrl | keyboard::Modifiers::SHIFT;
        assert!(matches!(key_binding(press("Z", shift), 0, false), Some(Binding::Custom(Message::Redo))));
        assert!(matches!(key_binding(press("y", ctrl), 0, false), Some(Binding::Custom(Message::Redo))));
        assert!(matches!(key_binding(press("a", ctrl), 0, false), Some(Binding::SelectAll)));
    }

    #[test]
    fn mention_tokens() {
        assert_eq!(mention_token("hi @sa"), Some((3, "sa")));
        assert_eq!(mention_token("@"), Some((0, "")));
        assert_eq!(mention_token("mail me@home"), None);
        assert_eq!(mention_token("@sam hi"), None);
    }

    #[test]
    fn ctrl_backspace_deletes_a_word() {
        let mut c: text_editor::Content = text_editor::Content::with_text("hello there friend");
        c.perform(Action::Move(Motion::DocumentEnd));
        let kp = KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Backspace),
            modifiers: if cfg!(target_os = "macos") { keyboard::Modifiers::ALT } else { keyboard::Modifiers::CTRL },
            text: None,
            status: Status::Focused { is_hovered: false },
        };
        let Some(Binding::Sequence(steps)) = key_binding(kp, 0, false) else { panic!("a word deletion") };
        for step in steps {
            match step {
                Binding::Select(m) => c.perform(Action::Select(m)),
                Binding::Backspace => c.perform(Action::Edit(Edit::Backspace)),
                _ => unreachable!(),
            }
        }
        assert_eq!(c.text().trim_end(), "hello there");
    }

    #[test]
    fn words_are_replaced_whole() {
        assert_eq!(replace_word("hi @sam and @samuel", "@sam", "<@1>"), "hi <@1> and @samuel");
        assert_eq!(replace_word("@sam.", "@sam", "<@1>"), "@sam.");
        assert_eq!(replace_word("@sam!", "@sam", "<@1>"), "<@1>!");
    }
}
