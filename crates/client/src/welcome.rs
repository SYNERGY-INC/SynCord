//! The first time Celerity is opened: pick a look, add a profile picture, and check the
//! microphone. Each step can be skipped, and it's all in Settings afterwards.

use iced::widget::{button, column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Length};

use crate::app::{App, DEFAULT_DEVICE, Message};
use crate::style::{self, bold, card, semibold};

/// Theme, picture, microphone.
pub const STEPS: u8 = 3;

impl App {
    pub(crate) fn welcome_view(&self, step: u8) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let name = self.session.as_ref().and_then(|s| s.me()).map(|u| u.name().to_string()).unwrap_or_default();
        let (title, subtitle) = match step {
            0 => (
                format!("Welcome to Celerity, {name}!"),
                "Let's make it yours. First, pick a look (there are more in Settings > Appearance).",
            ),
            1 => (
                "Add a profile picture".to_string(),
                "It shows next to your messages and in calls. Animated GIFs work too.",
            ),
            _ => (
                "Check your microphone".to_string(),
                "Talk for a moment: the bar turns green when others would hear you.",
            ),
        };
        let mut body = column![
            text(title).size(fs * 1.5).font(bold()).color(t.header.0),
            text(subtitle).size(fs * 0.95).color(t.muted.0),
        ]
        .spacing(8);

        match step {
            0 => {
                let mut grid = row![].spacing(12);
                for theme in self.themes.iter().take(6) {
                    grid = grid.push(crate::view::theme_tile(t, theme, theme.name == self.theme.name, fs));
                }
                body = body.push(container(grid.wrap().vertical_spacing(12)).padding([12, 0]));
            }
            1 => {
                let me = self.session.as_ref().and_then(|s| s.me());
                let mut r = row![].spacing(16).align_y(Alignment::Center);
                if let Some(u) = me {
                    r = r.push(self.avatar(u, 72.0));
                }
                r = r.push(
                    button(text("Choose a picture").size(fs).font(semibold()))
                        .padding([10, 18])
                        .on_press(Message::PickProfileImage(false))
                        .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                );
                body = body.push(container(r).padding([16, 0]));
            }
            _ => {
                let mut devices = vec![DEFAULT_DEVICE.to_string()];
                devices.extend(self.input_devices.iter().cloned());
                let current = self.settings.input_device.clone().unwrap_or_else(|| DEFAULT_DEVICE.to_string());
                let testing = self.mic_test.is_some();
                body = body.push(
                    column![
                        pick_list(devices, Some(current), Message::InputDevice).text_size(fs * 0.9).padding(10),
                        row![
                            button(text(if testing { "Stop" } else { "Let's check" }).size(fs * 0.95))
                                .padding([8, 16])
                                .on_press(Message::ToggleMicTest)
                                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
                            container(crate::view::level_meter(
                                t,
                                self.voice_status.input_level_db,
                                self.settings.input_sensitivity,
                                testing
                            ))
                            .width(Length::Fill),
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                        text("You'll hear yourself while testing. Push to talk, echo cancellation and more are in Settings > Voice & Video.")
                            .size(fs * 0.8)
                            .color(t.muted.0),
                    ]
                    .spacing(12)
                    .padding([12, 0]),
                );
            }
        }

        // Dots for the steps, then Skip / Next.
        let mut dots = row![].spacing(6).align_y(Alignment::Center);
        for i in 0..STEPS {
            let c = if i == step { t.accent.0 } else { t.hover.0 };
            dots = dots
                .push(container(space()).width(if i == step { 18 } else { 8 }).height(8).style(style::flat(c, 4.0)));
        }
        let last = step + 1 >= STEPS;
        let next = if last { Message::WelcomeDone } else { Message::WelcomeStep(step + 1) };
        body = body.push(
            row![
                dots,
                space::horizontal(),
                button(text("Skip").size(fs * 0.9))
                    .padding([8, 14])
                    .on_press(Message::WelcomeDone)
                    .style(style::link_button(t.muted.0, t.header.0)),
                button(text(if last { "Done" } else { "Next" }).size(fs * 0.95).font(semibold()))
                    .padding([8, 20])
                    .on_press(next)
                    .style(style::solid_button(t.accent.0, t.radius * 0.6)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        container(body).width(620).padding(28).style(card(t.chat.0, t.radius * 1.5)).into()
    }
}
