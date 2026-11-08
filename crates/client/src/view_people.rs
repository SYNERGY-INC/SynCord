//! The profile card.

use iced::widget::{column, container, row, space, stack, text};
use iced::{Alignment, Border, Element, Length, Padding};
use syncord_proto::User;

use crate::app::{App, Message, Session};
use crate::style::{self, BOLD, card, divider, label};
use crate::theme::{contrast_text, rgb};

const BANNER_HEIGHT: f32 = 128.0;
const AVATAR: f32 = 84.0;

/// Dot colours: green, yellow, red; grey for offline and invisible.
pub fn status_color(t: &crate::theme::AppTheme, status: Option<syncord_proto::Status>) -> iced::Color {
    use syncord_proto::Status::*;
    match status {
        Some(Online) => t.online.0,
        Some(Idle) => iced::Color::from_rgb8(0xF0, 0xB2, 0x32),
        Some(DoNotDisturb) => t.danger.0,
        Some(Invisible) | None => t.muted.0,
    }
}

impl App {
    /// Someone's status as shown on their dot (None: offline).
    pub(crate) fn status_of(&self, s: &Session, user: syncord_proto::UserId) -> Option<syncord_proto::Status> {
        if user == s.me {
            return Some(self.effective_status());
        }
        s.online.contains(&user).then(|| s.presences.get(&user).map(|p| p.0).unwrap_or_default())
    }

    /// "Playing Minecraft", if they're doing something.
    pub(crate) fn activity_of(&self, s: &Session, user: syncord_proto::UserId) -> Option<syncord_proto::Activity> {
        if user == s.me {
            return self.effective_activity().filter(|_| self.effective_status() != syncord_proto::Status::Invisible);
        }
        s.presences.get(&user).and_then(|p| p.1.clone())
    }

    /// Pick your status and what you're doing.
    pub(crate) fn status_picker<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        use syncord_proto::{ActivityKind, Status};
        let t = &self.theme;
        let fs = self.fs();
        let about = |st: Status| match st {
            Status::Online => "",
            Status::Idle => "Shown automatically after 10 minutes away",
            Status::DoNotDisturb => "No sounds or pop-ups for messages and calls",
            Status::Invisible => "You'll look offline, but can use SynCord as usual",
        };
        let mut list = column![text("Set your status").size(fs * 1.25).font(BOLD).color(t.header.0)].spacing(4);
        for st in Status::ALL {
            let dot = status_color(t, Some(st));
            let line = row![
                container(space()).width(12).height(12).style(move |_| container::Style {
                    background: Some(dot.into()),
                    border: Border { radius: 6.0.into(), ..Default::default() },
                    ..Default::default()
                }),
                column![
                    text(st.label()).size(fs * 0.95).color(t.header.0),
                    text(about(st)).size(fs * 0.75).color(t.muted.0)
                ]
                .spacing(1),
            ]
            .spacing(12)
            .align_y(Alignment::Center);
            list = list.push(
                iced::widget::button(line)
                    .width(Length::Fill)
                    .padding([7, 10])
                    .on_press(Message::SetStatus(st))
                    .style(style::row_button(t, self.settings.status == st)),
            );
        }
        let (kind, name) = &self.activity_draft;
        let activity = column![
            label(t, "What are you doing?", fs),
            row![
                iced::widget::pick_list(ActivityKind::ALL, Some(*kind), Message::ActivityKindPicked)
                    .text_size(fs * 0.9),
                iced::widget::text_input("e.g. Minecraft", name)
                    .on_input(Message::ActivityText)
                    .on_submit(Message::SaveActivity)
                    .padding(8)
                    .size(fs * 0.95)
                    .style(style::input(t, t.rail.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![
                iced::widget::button(text("Save").size(fs * 0.9))
                    .padding([6, 14])
                    .on_press(Message::SaveActivity)
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                iced::widget::button(text("Clear").size(fs * 0.9))
                    .padding([6, 10])
                    .on_press(Message::ClearActivity)
                    .style(style::link_button(t.text.0, t.header.0)),
            ]
            .spacing(8),
            iced::widget::toggler(self.settings.detect_games)
                .label("Show the game I'm playing (Steam, Epic, GOG, Riot, Xbox)")
                .on_toggle(Message::ToggleDetectGames)
                .text_size(fs * 0.85),
        ]
        .spacing(10);
        let mut body = column![list, divider(t.hover.0), activity].spacing(14);
        if let Some(game) = &self.detected_game {
            body = body.push(text(format!("Playing {game} right now")).size(fs * 0.8).color(t.muted.0));
        }
        let _ = s;
        container(body).width(420).padding(20).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// The bar under a direct message's header while a call is going on in it.
    pub(crate) fn call_bar<'a>(
        &'a self,
        s: &'a Session,
        channel: syncord_proto::ChannelId,
    ) -> Option<Element<'a, Message>> {
        let t = &self.theme;
        let fs = self.fs();
        let mut people: Vec<&User> =
            s.voice.values().filter(|v| v.channel_id == channel).filter_map(|v| s.users.get(&v.user_id)).collect();
        if people.is_empty() {
            return None;
        }
        people.sort_by_key(|u| u.id);
        let joined = self.voice.as_ref().is_some_and(|v| v.channel_id == channel);
        // With cameras or screens on, the call shows its video instead of faces.
        let video = joined && self.call_has_video(s, channel);
        let mut faces = row![].spacing(10).align_y(Alignment::Center);
        for u in people {
            let talking = self.speaking.contains(&u.id);
            let ring = if talking { t.online.0 } else { iced::Color::TRANSPARENT };
            faces = faces.push(container(self.avatar(u, 56.0)).padding(3).style(move |_| container::Style {
                border: Border { radius: 32.0.into(), width: 3.0, color: ring },
                ..Default::default()
            }));
        }
        let action = if joined {
            iced::widget::button(text("Leave Call").size(fs * 0.9))
                .padding([8, 16])
                .on_press(Message::LeaveVoice)
                .style(style::solid_button(t.danger.0, t.radius * 0.6))
        } else {
            iced::widget::button(text("Join Call").size(fs * 0.9))
                .padding([8, 16])
                .on_press(Message::JoinVoice(channel))
                .style(style::solid_button(t.online.0, t.radius * 0.6))
        };
        let top: Element<'a, Message> = if video { self.video_grid(s, channel, true) } else { faces.into() };
        let mut controls = iced::widget::row![action].spacing(10).align_y(Alignment::Center);
        if joined {
            controls = controls.push(container(self.live_buttons()).width(260));
        }
        Some(
            container(column![top, controls].spacing(14).align_x(Alignment::Center))
                .padding(18)
                .width(Length::Fill)
                .align_x(Alignment::Center)
                .style(style::bg(style::darken(t.chat.0, 0.35)))
                .into(),
        )
    }

    /// "So-and-so is calling" card, while a call rings.
    pub(crate) fn incoming_call(&self) -> Option<Element<'_, Message>> {
        let t = &self.theme;
        let fs = self.fs();
        let s = self.session.as_ref()?;
        let channel = self.ringing?;
        let caller = s.voice.values().find(|v| v.channel_id == channel && v.user_id != s.me)?;
        let u = s.users.get(&caller.user_id)?;
        let card_body = column![
            self.avatar(u, 72.0),
            text(u.name().to_owned()).size(fs * 1.15).font(BOLD).color(t.header.0),
            text("Incoming call...").size(fs * 0.85).color(t.muted.0),
            row![
                iced::widget::button(text("Dismiss").size(fs * 0.9))
                    .padding([8, 14])
                    .on_press(Message::DismissCall)
                    .style(style::solid_button(t.floating.0, t.radius * 0.6)),
                iced::widget::button(text("Join").size(fs * 0.9))
                    .padding([8, 18])
                    .on_press(Message::JoinVoice(channel))
                    .style(style::solid_button(t.online.0, t.radius * 0.6)),
            ]
            .spacing(10),
        ]
        .spacing(8)
        .align_x(Alignment::Center);
        Some(
            container(card_body)
                .padding(18)
                .width(240)
                .style(card(style::darken(t.floating.0, 0.1), t.radius * 1.5))
                .into(),
        )
    }

    /// Someone's profile card. `live`: play their animated avatar and banner.
    pub(crate) fn profile_card<'a>(
        &'a self,
        s: &'a Session,
        u: &User,
        online: bool,
        live: bool,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let radius = t.radius * 1.5;

        // The avatar sits over the bottom edge of the banner, with a ring of the card's colour.
        let floating = t.floating.0;
        let pic = container(if live { self.avatar_inner(u, AVATAR, true) } else { self.avatar(u, AVATAR) })
            .padding(5)
            .style(move |_| container::Style {
                background: Some(floating.into()),
                border: Border { radius: ((AVATAR + 10.0) / 2.0).into(), ..Default::default() },
                ..Default::default()
            });
        let dot_color = if online { status_color(t, self.status_of(s, u.id)) } else { t.muted.0 };
        let dot = container(space()).width(22).height(22).style(move |_| container::Style {
            background: Some(dot_color.into()),
            border: Border { radius: 11.0.into(), width: 4.0, color: floating },
            ..Default::default()
        });
        let head = stack![
            column![self.banner(u, BANNER_HEIGHT, radius), space().height(AVATAR / 2.0 + 8.0)],
            container(stack![pic, container(dot).padding(Padding { top: 66.0, left: 66.0, ..Padding::ZERO })])
                .padding(Padding { top: BANNER_HEIGHT - AVATAR / 2.0 - 5.0, left: 14.0, ..Padding::ZERO }),
        ];

        let mut body = column![
            column![
                text(u.name().to_owned()).size(fs * 1.35).font(BOLD).color(t.header.0),
                text(if u.profile.pronouns.is_empty() {
                    u.username.clone()
                } else {
                    format!("{} · {}", u.username, u.profile.pronouns)
                })
                .size(fs * 0.9)
                .color(t.muted.0),
            ]
            .spacing(2),
        ]
        .spacing(10);

        if u.is_owner {
            body = body.push(
                container(text("Server Owner").size(fs * 0.75).font(BOLD).color(contrast_text(t.accent.0)))
                    .padding([2, 8])
                    .style(card(t.accent.0, 10.0)),
            );
        }
        if let Some(a) = self.activity_of(s, u.id) {
            body = body.push(
                column![
                    label(t, a.kind.verb(), fs),
                    text(a.name).size(fs * 0.95).font(style::SEMIBOLD).color(t.header.0)
                ]
                .spacing(4),
            );
        }
        if !u.profile.status_text.trim().is_empty() {
            body = body.push(text(u.profile.status_text.clone()).size(fs * 0.95).color(t.text.0));
        }
        body = body.push(divider(t.hover.0));
        body = body.push(label(t, "About me", fs));
        let bio = if u.profile.bio.trim().is_empty() { "No bio yet.".to_string() } else { u.profile.bio.clone() };
        body = body.push(text(bio).size(fs * 0.95).color(t.text.0));

        let roles: Vec<_> = s.roles_of(u).collect();
        if !roles.is_empty() {
            body = body.push(label(t, "Roles", fs));
            let mut pills = row![].spacing(6);
            for r in roles {
                let dot = r.color.map(rgb).unwrap_or(t.muted.0);
                pills = pills.push(
                    container(
                        row![
                            container(space()).width(10).height(10).style(move |_| container::Style {
                                background: Some(dot.into()),
                                border: Border { radius: 5.0.into(), ..Default::default() },
                                ..Default::default()
                            }),
                            text(r.name.clone()).size(fs * 0.8).color(t.text.0),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    )
                    .padding([3, 8])
                    .style(card(style::darken(t.floating.0, 0.15), 6.0)),
                );
            }
            body = body.push(pills.wrap().vertical_spacing(6));
        }

        container(column![head, container(body).padding(Padding { top: 0.0, bottom: 14.0, left: 16.0, right: 16.0 })])
            .width(Length::Fixed(320.0))
            .style(card(t.floating.0, radius))
            .into()
    }
}
