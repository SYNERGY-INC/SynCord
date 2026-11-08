//! Update logic for emoji, reactions, direct messages and read markers.

use iced::Task;
use syncord_proto::{ClientMsg, Reaction, ServerMsg};

use crate::app::{App, EmojiPicker, Message, Overlay, PickerTab, PickerTarget, View};
use crate::emoji;

impl App {
    /// Emoji suggestions for a half-typed `:code` at the end of the message box.
    pub(crate) fn emoji_suggestions(&self) -> Vec<(&'static emojis::Emoji, &'static str)> {
        match emoji::autocomplete_token(&self.composer) {
            Some((_, code)) => emoji::suggestions(code, 6),
            None => Vec::new(),
        }
    }

    /// Replace the half-typed `:code` with the chosen emoji.
    pub(crate) fn accept_suggestion(&mut self, emoji: &str) {
        if let Some((start, _)) = emoji::autocomplete_token(&self.composer) {
            self.composer.truncate(start);
            self.composer.push_str(emoji);
            self.composer.push(' ');
            self.settings.remember_emoji(emoji);
            self.settings.save();
        }
    }

    /// Tell the server we've read the channel being shown, if there's anything new.
    pub(crate) fn mark_current_read(&mut self) {
        let Some(s) = &mut self.session else { return };
        let Some(channel) = s.current else { return };
        let newest_loaded = s.chans.get(&channel).and_then(|st| st.messages.last()).map(|m| m.id);
        let latest = s.latest.get(&channel).copied().max(newest_loaded);
        let Some(latest) = latest else { return };
        if s.read.get(&channel).is_some_and(|r| *r >= latest) {
            return;
        }
        s.read.insert(channel, latest);
        if let Some(conn) = &self.conn {
            conn.send(ClientMsg::MarkRead { channel_id: channel, message_id: latest });
        }
    }

    pub(crate) fn update_social(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenEmojiPicker(target) => {
                let same = self.emoji_picker.as_ref().is_some_and(|p| p.target == target);
                self.emoji_picker = if same {
                    None
                } else {
                    let tab = if self.settings.recent_emoji.is_empty() {
                        PickerTab::Emoji(0)
                    } else {
                        PickerTab::Emoji(usize::MAX)
                    };
                    Some(EmojiPicker { target, search: String::new(), tab })
                };
                return iced::widget::operation::focus(iced::widget::Id::new("emoji-search"));
            }
            Message::EmojiSearch(q) => {
                if let Some(p) = &mut self.emoji_picker {
                    p.search = q;
                }
            }
            Message::PickerTab(tab) => {
                if let Some(p) = &mut self.emoji_picker {
                    p.tab = tab;
                    p.search.clear();
                }
                if tab == PickerTab::Gifs {
                    return self.update_media(Message::AnimationTick);
                }
            }
            Message::PickEmoji(e) => {
                self.settings.remember_emoji(&e);
                self.settings.save();
                match self.emoji_picker.as_ref().map(|p| p.target) {
                    Some(PickerTarget::Reaction(message_id)) => {
                        self.emoji_picker = None;
                        if !self.has_reacted(message_id, &e) {
                            self.send(ClientMsg::AddReaction { message_id, emoji: e });
                        }
                    }
                    _ => {
                        // Keep the picker open so several emoji can be added.
                        self.composer.push_str(&e);
                        return iced::widget::operation::focus(iced::widget::Id::new(crate::app::COMPOSER_ID));
                    }
                }
            }
            Message::PickEmojiSuggestion(e) => {
                self.accept_suggestion(&e);
                return iced::widget::operation::focus(iced::widget::Id::new(crate::app::COMPOSER_ID));
            }
            Message::ToggleReaction(message_id, e) => {
                if self.has_reacted(message_id, &e) {
                    self.send(ClientMsg::RemoveReaction { message_id, emoji: e });
                } else {
                    self.settings.remember_emoji(&e);
                    self.settings.save();
                    self.send(ClientMsg::AddReaction { message_id, emoji: e });
                }
            }
            Message::ShowHome => {
                let Some(s) = &mut self.session else { return Task::none() };
                s.view = View::Home;
                let first = s.directs.first().map(|d| d.id);
                return match first {
                    Some(id) => self.select_channel(id),
                    None => {
                        s.current = None;
                        Task::none()
                    }
                };
            }
            Message::ShowServer => {
                let Some(s) = &self.session else { return Task::none() };
                let target = s
                    .last_server_channel
                    .filter(|id| s.channels.iter().any(|c| c.id == *id))
                    .or(s.first_text_channel());
                if let Some(id) = target {
                    return self.select_channel(id);
                }
            }
            Message::OpenDirect(user) => {
                self.close_overlay();
                let existing =
                    self.session.as_ref().and_then(|s| s.directs.iter().find(|d| d.user_id == user)).map(|d| d.id);
                match existing {
                    Some(id) => return self.select_channel(id),
                    None => {
                        self.pending_direct = Some(user);
                        self.send(ClientMsg::OpenDirect { user_id: user });
                    }
                }
            }
            Message::NewDirect => self.overlay = Some(Overlay::NewDirect),
            _ => {}
        }
        Task::none()
    }

    fn has_reacted(&self, message_id: syncord_proto::MessageId, e: &str) -> bool {
        let Some(s) = &self.session else { return false };
        s.chans
            .values()
            .flat_map(|st| &st.messages)
            .find(|m| m.id == message_id)
            .and_then(|m| m.reactions.iter().find(|r| r.emoji == e))
            .is_some_and(|r| r.users.contains(&s.me))
    }

    pub(crate) fn on_social(&mut self, msg: ServerMsg) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        match msg {
            ServerMsg::DirectOpened(d) => {
                if let Some(m) = d.last_message_id {
                    s.latest.entry(d.id).or_insert(m);
                }
                if !s.directs.iter().any(|x| x.id == d.id) {
                    s.directs.insert(0, d.clone());
                }
                if self.pending_direct == Some(d.user_id) {
                    self.pending_direct = None;
                    return self.select_channel(d.id);
                }
            }
            ServerMsg::ReactionUpdate { channel_id, message_id, emoji, user_id, added } => {
                if let Some(m) =
                    s.chans.get_mut(&channel_id).and_then(|st| st.messages.iter_mut().find(|m| m.id == message_id))
                {
                    match (added, m.reactions.iter().position(|r| r.emoji == emoji)) {
                        (true, Some(i)) => {
                            if !m.reactions[i].users.contains(&user_id) {
                                m.reactions[i].users.push(user_id);
                            }
                        }
                        (true, None) => m.reactions.push(Reaction { emoji, users: vec![user_id] }),
                        (false, Some(i)) => {
                            m.reactions[i].users.retain(|u| *u != user_id);
                            if m.reactions[i].users.is_empty() {
                                m.reactions.remove(i);
                            }
                        }
                        (false, None) => {}
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }
}
