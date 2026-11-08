//! People: roles and permissions, blocking and ignoring, mentions and pings.

use syncord_proto::{EVERYONE_ROLE, Message as ChatMessage, Role, RoleId, ServerMsg, User, UserId, perms};

use crate::app::{App, Session};

impl Session {
    pub fn role(&self, id: RoleId) -> Option<&Role> {
        self.roles.iter().find(|r| r.id == id)
    }

    /// Keep roles highest first, like the settings and member list show them.
    pub fn sort_roles(&mut self) {
        self.roles.sort_by(|a, b| b.position.cmp(&a.position).then(a.id.cmp(&b.id)));
    }

    /// What someone may do (the same rules the server uses).
    pub fn permissions_of(&self, user: UserId) -> u64 {
        let Some(u) = self.users.get(&user) else { return 0 };
        if u.is_owner {
            return perms::ALL;
        }
        let mut p = self.role(EVERYONE_ROLE).map_or(perms::DEFAULT, |r| r.permissions);
        for id in &u.roles {
            if let Some(r) = self.role(*id) {
                p |= r.permissions;
            }
        }
        if p & perms::ADMINISTRATOR != 0 { perms::ALL } else { p }
    }

    pub fn can(&self, permission: u64) -> bool {
        self.permissions_of(self.me) & permission == permission
    }

    /// Position of someone's highest role (0 for just @everyone).
    pub fn top_position(&self, user: UserId) -> i64 {
        let Some(u) = self.users.get(&user) else { return 0 };
        u.roles.iter().filter_map(|id| self.role(*id)).map(|r| r.position).max().unwrap_or(0)
    }

    /// Whether you can edit this role or hand it out.
    pub fn can_manage_role(&self, role: &Role) -> bool {
        role.id != EVERYONE_ROLE
            && self.can(perms::MANAGE_ROLES)
            && (self.is_owner() || role.position < self.top_position(self.me))
    }

    /// Someone's roles, highest first.
    pub fn roles_of<'a>(&'a self, u: &'a User) -> impl Iterator<Item = &'a Role> + 'a {
        self.roles.iter().filter(move |r| u.roles.contains(&r.id))
    }

    /// Name colour from someone's highest coloured role.
    pub fn role_color(&self, user: UserId) -> Option<u32> {
        let u = self.users.get(&user)?;
        self.roles_of(u).find_map(|r| r.color)
    }

    /// The highest role that lists people separately in the member list.
    pub fn hoisted_role(&self, user: UserId) -> Option<&Role> {
        let u = self.users.get(&user)?;
        self.roles_of(u).find(|r| r.hoist)
    }

    pub fn mentions_me(&self, m: &ChatMessage) -> bool {
        m.mentions.contains(&self.me)
    }

    /// Unread pings in a channel.
    pub fn mention_count(&self, channel: syncord_proto::ChannelId) -> u32 {
        self.mention_counts.get(&channel).copied().unwrap_or(0)
    }

    /// Unread pings in direct messages.
    pub fn direct_mentions(&self) -> u32 {
        self.directs.iter().map(|d| self.mention_count(d.id)).sum()
    }

    /// Unread pings in server channels.
    pub fn server_mentions(&self) -> u32 {
        self.channels.iter().map(|c| self.mention_count(c.id)).sum()
    }
}

impl App {
    /// Messages from people you've blocked or ignored are folded away.
    pub(crate) fn hides(&self, s: &Session, user: UserId) -> bool {
        s.blocked.contains(&user) || self.ignores(user)
    }

    pub(crate) fn ignores(&self, user: UserId) -> bool {
        self.settings.ignored_here(&self.settings.server).contains(&user)
    }

    pub(crate) fn on_people(&mut self, msg: ServerMsg) {
        let Some(s) = &mut self.session else { return };
        match msg {
            ServerMsg::RoleUpdated(role) => {
                match s.roles.iter_mut().find(|r| r.id == role.id) {
                    Some(slot) => *slot = role,
                    None => s.roles.push(role),
                }
                s.sort_roles();
                if let Some(edit) = &mut self.role_editing
                    && let Some(r) = s.roles.iter().find(|r| r.id == edit.id)
                    && !edit.dirty
                {
                    *edit = crate::app::RoleDraft::from_role(r);
                }
            }
            ServerMsg::RoleDeleted { role_id } => {
                s.roles.retain(|r| r.id != role_id);
                for u in s.users.values_mut() {
                    u.roles.retain(|r| *r != role_id);
                }
                if self.role_editing.as_ref().is_some_and(|r| r.id == role_id) {
                    self.role_editing = None;
                }
            }
            ServerMsg::BlockUpdated { user_id, blocked } => {
                if blocked {
                    s.blocked.insert(user_id);
                } else {
                    s.blocked.remove(&user_id);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn update_people(&mut self, message: crate::app::Message) -> iced::Task<crate::app::Message> {
        use crate::app::Message;
        use syncord_proto::ClientMsg;
        let Some(s) = &self.session else { return iced::Task::none() };
        match message {
            Message::StartCall(user) => {
                let existing = s.directs.iter().find(|d| d.user_id == user).map(|d| d.id);
                self.close_overlay();
                match existing {
                    Some(id) => {
                        let open = self.select_channel(id);
                        return iced::Task::batch([open, iced::Task::done(Message::JoinVoice(id))]);
                    }
                    None => {
                        self.pending_call = Some(user);
                        self.pending_direct = Some(user);
                        self.send(ClientMsg::OpenDirect { user_id: user });
                    }
                }
            }
            Message::ToggleIgnore(user) => {
                let ignored = !self.ignores(user);
                let server = self.settings.server.clone();
                self.settings.set_ignored(&server, user, ignored);
                self.settings.save();
            }
            Message::ToggleBlock(user) => {
                let blocked = !s.blocked.contains(&user);
                self.send(ClientMsg::SetBlocked { user_id: user, blocked });
            }
            Message::ToggleUserRole(user, role) => {
                if let Some(u) = s.users.get(&user) {
                    let mut roles = u.roles.clone();
                    if roles.contains(&role) {
                        roles.retain(|r| *r != role);
                    } else {
                        roles.push(role);
                    }
                    self.send(ClientMsg::SetUserRoles { user_id: user, roles });
                }
            }
            Message::CopyText(text) => {
                if self.clipboard.is_none() {
                    self.clipboard = arboard::Clipboard::new().ok();
                }
                if let Some(c) = self.clipboard.as_mut() {
                    let _ = c.set_text(text);
                }
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// A new message pings you: count it, and get your attention unless you're looking at it.
    pub(crate) fn on_mention(&mut self, m: &ChatMessage) -> iced::Task<crate::app::Message> {
        let Some(s) = &mut self.session else { return iced::Task::none() };
        if !s.mentions_me(m) || m.author_id == s.me || s.blocked.contains(&m.author_id) {
            return iced::Task::none();
        }
        let ignored = self.settings.ignored_here(&self.settings.server).contains(&m.author_id);
        if ignored {
            return iced::Task::none();
        }
        let looking = s.current == Some(m.channel_id) && self.focused;
        if looking {
            return iced::Task::none();
        }
        *s.mention_counts.entry(m.channel_id).or_default() += 1;
        if self.settings.status == syncord_proto::Status::DoNotDisturb {
            return iced::Task::none(); // counted, but quietly
        }
        if self.settings.ping_sound && !self.deafened {
            crate::sound::ping(self.settings.output_device.clone(), self.settings.sound_volume);
        }
        self.request_attention()
    }

    /// Flash the taskbar button (Windows), bounce the dock icon (macOS) or mark the window
    /// urgent (Linux) until SynCord is looked at.
    pub(crate) fn request_attention(&mut self) -> iced::Task<crate::app::Message> {
        self.wants_attention = true;
        iced::window::latest()
            .and_then(|id| iced::window::request_user_attention(id, Some(iced::window::UserAttention::Informational)))
    }
}
