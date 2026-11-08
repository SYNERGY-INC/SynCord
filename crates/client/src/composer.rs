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

impl App {
    /// Replace the message box text (cursor at the end).
    pub(crate) fn set_composer(&mut self, text: String) {
        self.editor = text_editor::Content::with_text(&text);
        self.editor.perform(Action::Move(Motion::DocumentEnd));
        self.composer = text;
        self.suggestion_index = 0;
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
                self.editor.perform(action);
                if edits {
                    self.composer = self.editor.text().trim_end_matches('\n').to_string();
                    self.suggestion_index = 0;
                    return self.typed();
                }
            }
            Message::SuggestionMove(step) => {
                let n = self.composer_suggestions().len() as i32;
                if n > 0 {
                    self.suggestion_index = (self.suggestion_index as i32 + step).rem_euclid(n) as usize;
                }
            }
            Message::AcceptSuggestion(i) => {
                if let Some(pick) = self.composer_suggestions().get(i).copied() {
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
    match kp.key.as_ref() {
        keyboard::Key::Named(Named::Enter) if kp.modifiers.shift() => Some(Binding::Enter),
        keyboard::Key::Named(Named::Enter) => Some(Binding::Custom(Message::SendMessage)),
        keyboard::Key::Named(Named::Tab) if suggestions > 0 => Some(Binding::Custom(Message::AcceptSuggestion(usize::MAX))),
        keyboard::Key::Named(Named::ArrowUp) if suggestions > 0 => Some(Binding::Custom(Message::SuggestionMove(-1))),
        keyboard::Key::Named(Named::ArrowDown) if suggestions > 0 => Some(Binding::Custom(Message::SuggestionMove(1))),
        keyboard::Key::Named(Named::ArrowUp) if empty => Some(Binding::Custom(Message::EditLast)),
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
    fn mention_tokens() {
        assert_eq!(mention_token("hi @sa"), Some((3, "sa")));
        assert_eq!(mention_token("@"), Some((0, "")));
        assert_eq!(mention_token("mail me@home"), None);
        assert_eq!(mention_token("@sam hi"), None);
    }

    #[test]
    fn words_are_replaced_whole() {
        assert_eq!(replace_word("hi @sam and @samuel", "@sam", "<@1>"), "hi <@1> and @samuel");
        assert_eq!(replace_word("@sam.", "@sam", "<@1>"), "@sam.");
        assert_eq!(replace_word("@sam!", "@sam", "<@1>"), "<@1>!");
    }
}
