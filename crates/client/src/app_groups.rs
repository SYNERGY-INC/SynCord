//! Group direct messages (0.7): conversations of three to ten people, listed with the
//! direct messages. Messages, pins, read markers and calls work like in a direct message.

use celerity_proto::social::{MAX_GROUP_NAME, MAX_GROUP_SIZE};
use celerity_proto::{ChannelId, ClientMsg, GroupChannel, ServerMsg, User, UserId};
use iced::Task;

use crate::app::{App, Message, Overlay, Session};
use crate::features::{ConfirmAction, Dialog};

/// Choosing people for a new conversation (one: a direct message; more: a group).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GroupPick {
    pub query: String,
    pub picked: Vec<UserId>,
}

/// A group's settings dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupEdit {
    pub id: ChannelId,
    pub name: String,
    /// Search box for adding people.
    pub query: String,
}

#[derive(Debug, Clone)]
pub enum GroupMsg {
    /// The + next to Conversations.
    OpenPicker,
    PickerQuery(String),
    TogglePick(UserId),
    Start,
    OpenSettings(ChannelId),
    Name(String),
    SaveName,
    Query(String),
    Add(UserId),
    Remove(UserId),
    AskLeave(ChannelId),
}

/// Most people you can pick besides yourself.
pub const MAX_PICK: usize = MAX_GROUP_SIZE - 1;

impl Session {
    pub fn group(&self, id: ChannelId) -> Option<&GroupChannel> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// A direct message or group: a conversation outside the server's channels.
    pub fn is_conversation(&self, id: ChannelId) -> bool {
        self.direct(id).is_some() || self.group(id).is_some()
    }

    /// Its name, or the other members' names when it has none.
    pub fn group_name(&self, g: &GroupChannel) -> String {
        if !g.name.trim().is_empty() {
            return g.name.clone();
        }
        let names: Vec<&str> =
            g.members.iter().filter(|id| **id != self.me).filter_map(|id| self.users.get(id).map(User::name)).collect();
        match names.as_slice() {
            [] => "Empty group".into(),
            _ => names.join(", "),
        }
    }

    /// Conversations (direct messages and groups), most recently active first.
    pub fn conversations(&self) -> Vec<ChannelId> {
        let latest = |id: ChannelId, fallback: Option<i64>| self.latest.get(&id).copied().or(fallback).unwrap_or(0);
        let mut all: Vec<(ChannelId, i64)> = self
            .directs
            .iter()
            .map(|d| (d.id, latest(d.id, d.last_message_id)))
            .chain(self.groups.iter().map(|g| (g.id, latest(g.id, g.last_message_id))))
            .collect();
        // Stable: conversations without messages keep their order.
        all.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
        all.into_iter().map(|(id, _)| id).collect()
    }
}

impl App {
    pub(crate) fn update_groups(&mut self, msg: GroupMsg) -> Task<Message> {
        match msg {
            GroupMsg::OpenPicker => {
                // Servers older than 0.7 only have two-person conversations.
                let v07 = self.session.as_ref().is_some_and(|s| s.v07);
                self.overlay = Some(if v07 {
                    Overlay::Dialog(Dialog::NewGroup(GroupPick::default()))
                } else {
                    Overlay::NewDirect
                });
                return iced::widget::operation::focus(iced::widget::Id::new("group-search"));
            }
            GroupMsg::PickerQuery(q) => {
                if let Some(Overlay::Dialog(Dialog::NewGroup(p))) = &mut self.overlay {
                    p.query = q;
                }
            }
            GroupMsg::TogglePick(user) => {
                if let Some(Overlay::Dialog(Dialog::NewGroup(p))) = &mut self.overlay {
                    if p.picked.contains(&user) {
                        p.picked.retain(|u| *u != user);
                    } else if p.picked.len() < MAX_PICK {
                        p.picked.push(user);
                    }
                }
            }
            GroupMsg::Start => {
                let Some(Overlay::Dialog(Dialog::NewGroup(p))) = self.overlay.clone() else { return Task::none() };
                match p.picked.as_slice() {
                    [] => {}
                    [one] => return self.update(Message::OpenDirect(*one)),
                    many if many.len() <= MAX_PICK => {
                        let Some(me) = self.session.as_ref().map(|s| s.me) else { return Task::none() };
                        let mut members = many.to_vec();
                        members.push(me);
                        members.sort_unstable();
                        self.pending_group = Some(members);
                        self.send(ClientMsg::CreateGroup { members: many.to_vec(), name: String::new() });
                        self.close_overlay();
                    }
                    _ => {}
                }
            }
            GroupMsg::OpenSettings(id) => {
                let Some(g) = self.session.as_ref().and_then(|s| s.group(id)) else { return Task::none() };
                let edit = GroupEdit { id, name: g.name.clone(), query: String::new() };
                self.close_overlay();
                self.overlay = Some(Overlay::Dialog(Dialog::GroupSettings(edit)));
            }
            GroupMsg::Name(v) => {
                if let Some(Overlay::Dialog(Dialog::GroupSettings(g))) = &mut self.overlay {
                    g.name = v.chars().take(MAX_GROUP_NAME).collect();
                }
            }
            GroupMsg::SaveName => {
                if let Some(Overlay::Dialog(Dialog::GroupSettings(g))) = &self.overlay {
                    let (channel_id, name) = (g.id, g.name.trim().to_string());
                    self.send(ClientMsg::RenameGroup { channel_id, name });
                }
            }
            GroupMsg::Query(q) => {
                if let Some(Overlay::Dialog(Dialog::GroupSettings(g))) = &mut self.overlay {
                    g.query = q;
                }
            }
            GroupMsg::Add(user_id) => {
                if let Some(Overlay::Dialog(Dialog::GroupSettings(g))) = &mut self.overlay {
                    g.query.clear();
                    let channel_id = g.id;
                    self.send(ClientMsg::AddGroupMember { channel_id, user_id });
                }
            }
            GroupMsg::Remove(user_id) => {
                if let Some(Overlay::Dialog(Dialog::GroupSettings(g))) = &self.overlay {
                    self.send(ClientMsg::RemoveGroupMember { channel_id: g.id, user_id });
                }
            }
            GroupMsg::AskLeave(id) => {
                let Some(name) = self.session.as_ref().and_then(|s| s.group(id).map(|g| s.group_name(g))) else {
                    return Task::none();
                };
                self.ask(
                    "Leave group",
                    format!("Leave {name}? You won't see its messages any more unless someone adds you back."),
                    "Leave Group",
                    ConfirmAction::LeaveGroup(id),
                );
            }
        }
        Task::none()
    }

    pub(crate) fn on_group_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let background = self.background;
        let Some(s) = &mut self.session else { return Task::none() };
        match msg {
            ServerMsg::GroupUpdated(g) => {
                if let Some(m) = g.last_message_id {
                    s.latest.entry(g.id).or_insert(m);
                }
                let id = g.id;
                let mut members = g.members.clone();
                members.sort_unstable();
                match s.groups.iter_mut().find(|x| x.id == g.id) {
                    Some(slot) => *slot = g,
                    None => s.groups.push(g),
                }
                if !background && self.pending_group.as_ref() == Some(&members) {
                    self.pending_group = None;
                    return self.select_channel(id);
                }
            }
            ServerMsg::GroupRemoved { channel_id } => {
                s.groups.retain(|g| g.id != channel_id);
                s.chans.remove(&channel_id);
                if matches!(&self.overlay, Some(Overlay::Dialog(Dialog::GroupSettings(g))) if g.id == channel_id)
                    && !background
                {
                    self.overlay = None;
                }
                if s.current == Some(channel_id) {
                    s.current = None;
                    if !background {
                        return self.update(Message::ShowHome);
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }
}
