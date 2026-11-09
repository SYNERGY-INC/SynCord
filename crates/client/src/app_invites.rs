//! Invite links and moderation (0.7): making and revoking invites, opening
//! `celerity://join/...` links, and kicking or banning people.

use celerity_proto::invites::{invite_link, parse_invite_link};
use celerity_proto::{ClientMsg, Invite, ServerMsg, User, UserId, perms};
use iced::widget::{button, column, container, pick_list, row, scrollable, space, text};
use iced::{Alignment, Element, Length, Task};

use crate::app::{App, Message, Overlay, Session};
use crate::features::{ConfirmAction, Dialog};
use crate::icons;
use crate::style::{self, bold, card, label};

/// How long a new invite works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Expiry {
    HalfHour,
    Hour,
    Day,
    #[default]
    Week,
    Never,
}

impl Expiry {
    pub const ALL: [Expiry; 5] = [Self::HalfHour, Self::Hour, Self::Day, Self::Week, Self::Never];

    pub fn secs(self) -> Option<u64> {
        match self {
            Self::HalfHour => Some(1800),
            Self::Hour => Some(3600),
            Self::Day => Some(86_400),
            Self::Week => Some(7 * 86_400),
            Self::Never => None,
        }
    }
}

impl std::fmt::Display for Expiry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HalfHour => "30 minutes",
            Self::Hour => "1 hour",
            Self::Day => "1 day",
            Self::Week => "7 days",
            Self::Never => "Never",
        })
    }
}

/// How many accounts a new invite can make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Uses(pub Option<u32>);

impl Uses {
    pub const ALL: [Uses; 7] =
        [Uses(None), Uses(Some(1)), Uses(Some(5)), Uses(Some(10)), Uses(Some(25)), Uses(Some(50)), Uses(Some(100))];
}

impl std::fmt::Display for Uses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("No limit"),
            Some(1) => f.write_str("1 use"),
            Some(n) => write!(f, "{n} uses"),
        }
    }
}

#[derive(Default)]
pub struct InviteUi {
    /// Invites that still work (once the server has answered).
    pub list: Option<Vec<Invite>>,
    /// The one just made, shown with its links.
    pub created: Option<Invite>,
    pub expiry: Expiry,
    pub uses: Uses,
    pub(crate) waiting: bool,
}

#[derive(Debug, Clone)]
pub enum InviteMsg {
    Open,
    Expiry(Expiry),
    Uses(Uses),
    Create,
    Revoke(String),
    Kick(UserId),
    Ban(UserId),
}

/// The web page for an invite, for places where `celerity://` links can't be clicked:
/// `http(s)://<address>/invite/<token>` (https when the server is reached encrypted).
pub fn web_invite_link(server_base: &str, address: &str, token: &str) -> String {
    let scheme = if server_base.starts_with("https://") { "https" } else { "http" };
    format!("{scheme}://{}{}/{token}", address.trim().trim_end_matches('/'), celerity_proto::invites::INVITE_PAGE_PATH)
}

/// "Expires in 3 hours", "Never expires", with "· 2 of 5 uses".
pub fn describe(invite: &Invite, now: i64) -> String {
    let expiry = match invite.expires_at {
        None => "Never expires".to_string(),
        Some(t) if t <= now => "Expired".to_string(),
        Some(t) => format!("Expires in {}", crate::polls::time_left(t - now)),
    };
    match invite.max_uses {
        Some(max) => format!("{expiry} · {} of {max} uses", invite.uses),
        None if invite.uses == 1 => format!("{expiry} · used once"),
        None => format!("{expiry} · used {} times", invite.uses),
    }
}

impl Session {
    /// You may remove this person (the server checks the same: ranked below you).
    pub fn can_remove(&self, u: &User, permission: u64) -> bool {
        self.v07
            && u.id != self.me
            && !u.removed
            && !u.is_owner
            && self.can(permission)
            && (self.is_owner() || self.top_position(self.me) > self.top_position(u.id))
    }
}

impl App {
    /// The address invite links use: the one the owner set, or the one we connected to.
    pub(crate) fn invite_address(&self) -> String {
        self.session
            .as_ref()
            .and_then(|s| s.public_address.clone())
            .unwrap_or_else(|| crate::net::server_key(&self.settings.server))
    }

    pub(crate) fn update_invites(&mut self, msg: InviteMsg) -> Task<Message> {
        match msg {
            InviteMsg::Open => {
                self.server_menu = false;
                self.settings_tab = None;
                self.close_overlay();
                self.invites.created = None;
                self.invites.list = None;
                self.overlay = Some(Overlay::Dialog(Dialog::Invite));
                self.send(ClientMsg::ListInvites);
            }
            InviteMsg::Expiry(e) => self.invites.expiry = e,
            InviteMsg::Uses(u) => self.invites.uses = u,
            InviteMsg::Create => {
                self.invites.waiting = true;
                self.send(ClientMsg::CreateInvite {
                    max_age_secs: self.invites.expiry.secs(),
                    max_uses: self.invites.uses.0,
                });
            }
            InviteMsg::Revoke(token) => self.send(ClientMsg::RevokeInvite { token }),
            InviteMsg::Kick(id) | InviteMsg::Ban(id) => {
                let ban = matches!(msg, InviteMsg::Ban(_));
                let Some(name) = self.session.as_ref().and_then(|s| s.users.get(&id)).map(|u| u.name().to_string())
                else {
                    return Task::none();
                };
                if ban {
                    self.ask(
                        format!("Ban {name}"),
                        format!(
                            "{name} will be signed out and can't come back until someone unbans them (on the admin \
                             page). Their messages stay."
                        ),
                        "Ban",
                        ConfirmAction::Ban(id),
                    );
                } else {
                    self.ask(
                        format!("Kick {name}"),
                        format!(
                            "{name} will be signed out. They can come back with a new invite. Their messages stay."
                        ),
                        "Kick",
                        ConfirmAction::Kick(id),
                    );
                }
            }
        }
        Task::none()
    }

    pub(crate) fn on_invite_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let me = self.session.as_ref().map(|s| s.me);
        match msg {
            ServerMsg::InviteCreated(invite) => {
                self.invites.waiting = false;
                if let Some(list) = &mut self.invites.list {
                    list.insert(0, invite.clone());
                }
                self.invites.created = Some(invite);
            }
            ServerMsg::Invites { invites } => self.invites.list = Some(invites),
            ServerMsg::InviteRevoked { token } => {
                if let Some(list) = &mut self.invites.list {
                    list.retain(|i| i.token != token);
                }
                if self.invites.created.as_ref().is_some_and(|i| i.token == token) {
                    self.invites.created = None;
                }
            }
            ServerMsg::MemberRemoved { user_id, banned } if Some(user_id) == me => {
                // The server signs us out next; say why.
                self.notice = Some(if banned {
                    "You were banned from this server.".into()
                } else {
                    "You were removed from this server. You can come back with a new invite.".into()
                });
            }
            _ => {}
        }
        Task::none()
    }

    /// A `celerity://join/<address>/<token>` link: show that server if you're signed in to
    /// it, otherwise start registering there with the invite filled in.
    pub(crate) fn open_invite(&mut self, link: &str) -> Task<Message> {
        let Some((address, token)) = parse_invite_link(link) else { return Task::none() };
        let key = crate::net::server_key(&address);
        if let Some(e) = self.rail().into_iter().find(|e| e.key == key && !e.offline) {
            return if e.open { self.update(Message::ShowServer) } else { self.switch_server(e.position) };
        }
        if self.session.is_some() {
            self.add_server();
        }
        self.login.server = address;
        self.login.invite = token;
        self.login.register = true;
        self.login.error = None;
        Task::none()
    }

    pub(crate) fn invite_dialog<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let im = Message::Invites;
        let mut body = column![
            row![
                icons::icon(&icons::INVITE, fs * 1.4, t.accent.0),
                text(format!("Invite people to {}", s.community)).size(fs * 1.3).font(bold()).color(t.header.0),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text("Anyone with the link can make an account here. Celerity opens with everything filled in.")
                .size(fs * 0.85)
                .color(t.muted.0),
            row![
                column![
                    label(t, "Expire after", fs),
                    pick_list(Expiry::ALL, Some(self.invites.expiry), move |e| im(InviteMsg::Expiry(e)))
                        .text_size(fs * 0.9)
                        .padding(8)
                ]
                .spacing(6),
                column![
                    label(t, "Max uses", fs),
                    pick_list(Uses::ALL, Some(self.invites.uses), move |u| im(InviteMsg::Uses(u)))
                        .text_size(fs * 0.9)
                        .padding(8)
                ]
                .spacing(6),
                space::horizontal(),
                button(text(if self.invites.waiting { "Creating..." } else { "Create Link" }).size(fs))
                    .padding([8, 16])
                    .on_press_maybe((!self.invites.waiting).then_some(im(InviteMsg::Create)))
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(12)
            .align_y(Alignment::End),
        ]
        .spacing(12);

        if let Some(invite) = &self.invites.created {
            let address = self.invite_address();
            let app_link = invite_link(&address, &invite.token);
            let web_link = web_invite_link(&self.server_base(), &address, &invite.token);
            let link_box = |title: &'static str, hint: &'static str, link: String| {
                column![
                    label(t, title, fs),
                    row![
                        container(text(link.clone()).size(fs * 0.85).font(iced::Font::MONOSPACE).color(t.header.0))
                            .padding([8, 10])
                            .width(Length::Fill)
                            .style(card(t.rail.0, t.radius * 0.6)),
                        button(text("Copy").size(fs * 0.9))
                            .padding([8, 14])
                            .on_press(Message::CopyText(link))
                            .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                    text(hint).size(fs * 0.75).color(t.muted.0),
                ]
                .spacing(6)
            };
            body = body.push(link_box("Invite link", "Opens Celerity straight away.", app_link));
            body = body.push(link_box(
                "Web link",
                "For places where the link above isn't clickable: a page with an Open in Celerity button and a download link.",
                web_link,
            ));
            body = body
                .push(text(describe(invite, crate::app::now_ms() + s.clock_offset_ms)).size(fs * 0.8).color(t.muted.0));
        }

        body = body.push(style::divider(t.hover.0)).push(label(t, "Invites that still work", fs));
        let now = crate::app::now_ms() + s.clock_offset_ms;
        let mut list = column![].spacing(4);
        match &self.invites.list {
            None => list = list.push(text("Loading...").size(fs * 0.85).color(t.muted.0)),
            Some(l) if l.is_empty() => list = list.push(text("None yet.").size(fs * 0.85).color(t.muted.0)),
            Some(l) => {
                for i in l {
                    let by = s.users.get(&i.created_by).map(User::name).unwrap_or("someone");
                    let danger = t.danger.0;
                    list = list.push(
                        container(
                            row![
                                column![
                                    text(i.token.clone()).size(fs * 0.85).font(iced::Font::MONOSPACE).color(t.header.0),
                                    text(format!("By {by} · {}", describe(i, now))).size(fs * 0.75).color(t.muted.0),
                                ]
                                .spacing(2)
                                .width(Length::Fill),
                                button(text("Copy").size(fs * 0.8))
                                    .padding([4, 10])
                                    .on_press(Message::CopyText(invite_link(&self.invite_address(), &i.token)))
                                    .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                                button(text("Revoke").size(fs * 0.8))
                                    .padding([4, 10])
                                    .on_press(im(InviteMsg::Revoke(i.token.clone())))
                                    .style(style::link_button(danger, style::darken(danger, 0.2))),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        )
                        .padding([6, 10])
                        .style(card(t.floating.0, t.radius * 0.6)),
                    );
                }
            }
        }
        body = body.push(container(scrollable(list).height(Length::Shrink).style(style::scroll(t))).max_height(200));
        body = body.push(
            row![
                text("The old invite code keeps working too.").size(fs * 0.75).color(t.muted.0),
                space::horizontal(),
                button(text("Done").size(fs))
                    .padding([8, 16])
                    .on_press(Message::CloseOverlay)
                    .style(style::link_button(t.text.0, t.header.0)),
            ]
            .align_y(Alignment::Center),
        );
        container(body).width(540).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// Kick and Ban in a member's right-click menu, for people allowed to.
    pub(crate) fn moderation_items(&self, s: &Session, u: &User) -> Vec<(String, Message)> {
        let mut items = Vec::new();
        if s.can_remove(u, perms::KICK_MEMBERS) {
            items.push((format!("Kick {}", u.name()), Message::Invites(InviteMsg::Kick(u.id))));
        }
        if s.can_remove(u, perms::BAN_MEMBERS) {
            items.push((format!("Ban {}", u.name()), Message::Invites(InviteMsg::Ban(u.id))));
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_links() {
        assert_eq!(
            web_invite_link("https://chat.example.com:7878", "chat.example.com:7878", "AbC"),
            "https://chat.example.com:7878/invite/AbC"
        );
        assert_eq!(
            web_invite_link("http://10.0.0.2:7878", "friends.example.com/", "x1"),
            "http://friends.example.com/invite/x1"
        );
        // What the app makes, it can open again.
        let link = invite_link("10.0.0.2:7878", "Tok3n");
        assert_eq!(parse_invite_link(&link), Some(("10.0.0.2:7878".into(), "Tok3n".into())));
        assert_eq!(
            crate::net::server_key("10.0.0.2"),
            crate::net::server_key("10.0.0.2:7878"),
            "the default port matches"
        );
    }

    #[test]
    fn invite_descriptions() {
        let i = Invite {
            token: "t".into(),
            created_by: 1,
            created_at: 0,
            expires_at: Some(3_600_000),
            max_uses: Some(5),
            uses: 2,
        };
        assert_eq!(describe(&i, 0), "Expires in 1h · 2 of 5 uses");
        assert_eq!(describe(&i, 3_600_000), "Expired · 2 of 5 uses");
        let forever = Invite { expires_at: None, max_uses: None, uses: 1, ..i };
        assert_eq!(describe(&forever, 0), "Never expires · used once");
        assert_eq!(Uses(Some(1)).to_string(), "1 use");
        assert_eq!(Expiry::Never.secs(), None);
        assert_eq!(Expiry::HalfHour.secs(), Some(1800));
    }
}
