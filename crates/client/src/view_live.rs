//! Video in calls: the camera and screen buttons, the tiles people's video
//! shows in, the call view for server voice channels, and the screen picker.

use iced::widget::{button, column, container, image, mouse_area, row, scrollable, space, text, toggler};
use iced::{Alignment, Border, Color, ContentFit, Element, Length, Padding};
use syncord_proto::{ChannelId, User, UserId, VideoKind, VoiceState};

use crate::app::{App, Message, Session};
use crate::icons;
use crate::live::ScreenList;
use crate::style::{self, bold, card, semibold};
use crate::theme::with_alpha;

/// A tile is a person's camera (or their avatar) or one of their screens.
#[derive(Clone, Copy)]
struct Tile<'a> {
    user: &'a User,
    state: &'a VoiceState,
    kind: VideoKind,
}

impl App {
    /// Camera and Screen buttons under "Voice Connected".
    pub(crate) fn live_buttons(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let ready = self.voice.is_some();
        let make = |kind: VideoKind| {
            let on = self.live.sending(kind);
            let starting = self.live.starting.contains(&kind);
            let (icon, label, msg) = match kind {
                VideoKind::Camera => (&icons::CAMERA, if on { "Camera on" } else { "Camera" }, Message::ToggleCamera),
                VideoKind::Screen => {
                    (&icons::SCREEN, if on { "Stop sharing" } else { "Screen" }, Message::OpenScreenPicker)
                }
            };
            let label = if starting { "Starting..." } else { label };
            let fg = if on { Color::WHITE } else { t.header.0 };
            let fill = if on { t.online.0 } else { style::darken(t.user_panel.0, -0.08) };
            button(
                row![
                    icons::icon(icon, fs * 1.05, fg),
                    text(label).size(fs * 0.8).color(fg).wrapping(iced::widget::text::Wrapping::None)
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([6, 8])
            .on_press_maybe((ready && !starting).then_some(msg))
            .style(style::solid_button(fill, 6.0))
        };
        row![make(VideoKind::Camera), make(VideoKind::Screen)].spacing(8).into()
    }

    /// Everyone in a call's tiles: a camera tile per person (their avatar while
    /// it's off) and a tile per screen share.
    fn tiles<'a>(&'a self, s: &'a Session, channel: ChannelId) -> Vec<Tile<'a>> {
        let mut people: Vec<(&User, &VoiceState)> = s
            .voice
            .values()
            .filter(|v| v.channel_id == channel)
            .filter_map(|v| Some((s.users.get(&v.user_id)?, v)))
            .collect();
        people.sort_by_key(|(u, _)| (u.id != s.me, u.name().to_lowercase()));
        let mut out = Vec::new();
        for (user, state) in &people {
            if state.screen {
                out.push(Tile { user, state, kind: VideoKind::Screen });
            }
        }
        for (user, state) in people {
            out.push(Tile { user, state, kind: VideoKind::Camera });
        }
        out
    }

    /// Whether anyone in this call has a camera or screen on.
    pub(crate) fn call_has_video(&self, s: &Session, channel: ChannelId) -> bool {
        s.voice.values().any(|v| v.channel_id == channel && (v.camera || v.screen))
    }

    fn tile<'a>(
        &'a self,
        s: &'a Session,
        tile: Tile<'a>,
        width: impl Into<Length>,
        height: impl Into<Length>,
        avatar: f32,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let Tile { user, state, kind } = tile;
        let mine = user.id == s.me;
        let key = (user.id, kind);
        let picture = self.live.pictures.get(&key);
        // Cameras are always shown; screens once you choose to watch them.
        let watching = kind == VideoKind::Camera || mine || self.live.watching.contains(&user.id);

        let on = kind == VideoKind::Screen || state.camera;
        let body: Element<'a, Message> = if !watching {
            container(
                column![
                    text(format!("{} is sharing their screen", user.name())).size(fs * 0.9).color(Color::WHITE),
                    button(text("Watch Stream").size(fs * 0.9).font(semibold()))
                        .padding([8, 16])
                        .on_press(Message::WatchScreen(user.id))
                        .style(style::solid_button(t.accent.0, 6.0)),
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .center(Length::Fill)
            .into()
        } else if !on {
            container(self.avatar(user, avatar)).center(Length::Fill).into()
        } else if let Some(p) = picture {
            // Faces fill their tile; screens show whole.
            let fit = if kind == VideoKind::Camera { ContentFit::Cover } else { ContentFit::Contain };
            image(p.handle.clone()).content_fit(fit).width(Length::Fill).height(Length::Fill).into()
        } else {
            container(text("Loading video...").size(fs * 0.85).color(with_alpha(Color::WHITE, 0.7)))
                .center(Length::Fill)
                .into()
        };

        // Name (and mic state) in the corner, like Discord.
        let mut tag = row![].spacing(4).align_y(Alignment::Center);
        if state.muted || state.deafened {
            tag = tag.push(icons::icon(&icons::MIC_OFF, fs * 0.8, t.danger.0));
        }
        let name = match kind {
            VideoKind::Camera => user.name().to_string(),
            VideoKind::Screen if mine => "Your screen".to_string(),
            VideoKind::Screen => format!("{}'s screen", user.name()),
        };
        tag = tag.push(text(name).size(fs * 0.78).font(semibold()).color(Color::WHITE));
        let mut corner = row![
            container(tag).padding([3, 7]).style(style::flat(Color::from_rgba(0.0, 0.0, 0.0, 0.6), 4.0)),
            space::horizontal(),
        ];
        if kind == VideoKind::Screen && !mine && watching {
            corner = corner.push(
                button(text("Stop watching").size(fs * 0.72).color(Color::WHITE))
                    .padding([3, 7])
                    .on_press(Message::StopWatching(user.id))
                    .style(style::solid_button(Color::from_rgba(0.0, 0.0, 0.0, 0.6), 4.0)),
            );
        }
        let overlay = container(corner).padding(8).height(Length::Fill).align_y(Alignment::End);

        let talking = kind == VideoKind::Camera && self.speaking.contains(&user.id);
        let ring = if talking { t.online.0 } else { Color::TRANSPARENT };
        let fill = style::darken(t.rail.0, 0.2);
        let framed =
            container(iced::widget::stack![body, overlay]).width(width).height(height).clip(true).style(move |_| {
                container::Style {
                    background: Some(fill.into()),
                    border: Border { radius: 8.0.into(), width: 2.0, color: ring },
                    ..Default::default()
                }
            });
        mouse_area(framed).on_press(Message::FocusVideo(Some(key))).into()
    }

    /// The tiles laid out: a grid, or one big with the rest in a strip below.
    pub(crate) fn video_grid<'a>(&'a self, s: &'a Session, channel: ChannelId, compact: bool) -> Element<'a, Message> {
        let tiles = self.tiles(s, channel);
        let focused = self.live.focus.and_then(|(u, k)| tiles.iter().copied().find(|x| x.user.id == u && x.kind == k));
        let avatar = |h: f32| (h * 0.38).clamp(28.0, 96.0);
        if let Some(big) = focused {
            let small_w = if compact { 140.0 } else { 176.0 };
            let small_h = small_w * 9.0 / 16.0;
            let mut strip = row![].spacing(8);
            for x in tiles.iter().copied().filter(|x| !(x.user.id == big.user.id && x.kind == big.kind)) {
                strip = strip.push(self.tile(s, x, small_w, small_h, avatar(small_h)));
            }
            // The big one takes the space there is (a fixed height in a DM's call bar).
            let big_h = if compact { Length::Fixed(300.0) } else { Length::Fill };
            return column![
                self.tile(s, big, Length::Fill, big_h, 96.0),
                container(scrollable(strip).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new())))
                    .center_x(Length::Fill),
            ]
            .spacing(10)
            .into();
        }
        let w = match (compact, tiles.len()) {
            (true, _) => 240.0,
            (false, 0..=1) => 640.0,
            (false, 2..=4) => 400.0,
            _ => 300.0,
        };
        let mut grid = row![].spacing(10);
        for x in tiles {
            grid = grid.push(self.tile(s, x, w, w * 9.0 / 16.0, avatar(w * 9.0 / 16.0)));
        }
        container(grid.wrap().vertical_spacing(10).align_x(Alignment::Center)).center_x(Length::Fill).into()
    }

    /// The call view for a server voice channel (in place of the chat).
    pub(crate) fn stage_view<'a>(&'a self, s: &'a Session, channel: ChannelId) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let name = s.channels.iter().find(|c| c.id == channel).map(|c| c.name.clone()).unwrap_or_default();
        let header = container(
            row![
                icons::icon(&icons::SPEAKER, fs * 1.3, t.muted.0),
                text(name).size(fs * 1.05).font(bold()).color(t.header.0),
                space::horizontal(),
                button(text("Back to chat").size(fs * 0.8))
                    .padding([4, 8])
                    .on_press(Message::CloseStage)
                    .style(style::icon_button(t)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([0, 16])
        .height(48)
        .align_y(Alignment::Center);

        let controls = row![
            container(self.live_buttons()).width(300),
            button(icons::icon(&icons::HANG_UP, fs * 1.3, Color::WHITE))
                .padding([6, 14])
                .on_press(Message::LeaveVoice)
                .style(style::solid_button(t.danger.0, 6.0)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        // One video big fills the stage; a grid scrolls if it doesn't fit.
        let grid = self.video_grid(s, channel, false);
        let body: Element<'a, Message> = if self.live.focus.is_some() {
            container(grid).padding(16).height(Length::Fill).into()
        } else {
            container(scrollable(container(grid).padding(16)).style(style::scroll(t)))
                .height(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };
        container(column![
            header,
            body,
            container(controls).center_x(Length::Fill).padding(Padding { top: 8.0, bottom: 16.0, ..Padding::ZERO }),
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Choosing a screen (or a window, on macOS) to share.
    pub(crate) fn screen_picker<'a>(&'a self, list: &'a ScreenList) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut col = column![text("Share your screen").size(fs * 1.25).font(bold()).color(t.header.0)].spacing(12);
        match list {
            ScreenList::Loading => col = col.push(text("Looking for screens...").size(fs * 0.9).color(t.muted.0)),
            ScreenList::Failed(e) => col = col.push(text(e.clone()).size(fs * 0.9).color(t.danger.0)),
            ScreenList::Ready(screens) => {
                let mut items = column![].spacing(6);
                for sc in screens {
                    let kind = match sc.kind {
                        syncord_stream::ScreenKind::Display => "Screen",
                        syncord_stream::ScreenKind::Window => "Window",
                    };
                    items = items.push(
                        button(
                            row![
                                icons::icon(&icons::SCREEN, fs * 1.3, t.muted.0),
                                column![
                                    text(sc.name.clone()).size(fs * 0.95).color(t.header.0),
                                    text(format!("{kind} · {}×{}", sc.width, sc.height))
                                        .size(fs * 0.75)
                                        .color(t.muted.0),
                                ]
                                .spacing(2),
                            ]
                            .spacing(10)
                            .align_y(Alignment::Center),
                        )
                        .width(Length::Fill)
                        .padding(10)
                        .on_press(Message::ShareScreen(sc.id.clone()))
                        .style(style::row_button(t, false)),
                    );
                }
                col = col.push(scrollable(items).height(Length::Shrink).style(style::scroll(t)));
            }
        }
        col = col.push(
            toggler(self.live.smooth)
                .label("Smooth motion (720p, 30 fps) instead of sharp text (1080p, 15 fps)")
                .text_size(fs * 0.85)
                .on_toggle(Message::ToggleSmoothScreen),
        );
        col = col.push(row![
            space::horizontal(),
            button(text("Cancel").size(fs * 0.9))
                .padding([8, 16])
                .on_press(Message::CloseOverlay)
                .style(style::solid_button(t.floating.0, 6.0)),
        ]);
        container(col).width(460).max_height(520).padding(20).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// Camera and "LIVE" badges next to someone under a voice channel.
    pub(crate) fn live_badges<'a>(&'a self, v: &VoiceState, me: UserId) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut r = row![].spacing(4).align_y(Alignment::Center);
        if v.camera {
            r = r.push(icons::icon(&icons::CAMERA, fs * 0.9, t.muted.0));
        }
        if v.screen {
            let pill = container(text("LIVE").size(fs * 0.62).font(bold()).color(Color::WHITE))
                .padding([1, 5])
                .style(style::flat(t.danger.0, 4.0));
            let in_my_call = self.voice.as_ref().is_some_and(|mine| mine.channel_id == v.channel_id);
            r = r.push(if in_my_call && v.user_id != me {
                Element::from(
                    button(pill).padding(0).on_press(Message::WatchScreen(v.user_id)).style(style::icon_button(t)),
                )
            } else {
                pill.into()
            });
        }
        r.into()
    }
}
