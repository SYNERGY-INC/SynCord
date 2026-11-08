//! Pop-out colour picker: a saturation/brightness square, a hue bar, a hex box and
//! preset swatches. Built from gradient-filled boxes, so it needs no drawing canvas.

use iced::widget::{column, container, mouse_area, pin, responsive, row, stack, text, text_input};
use iced::{Border, Color, Element, Length, Point, Radians, Task, gradient};

use crate::app::{App, Message};
use crate::style::{self, card};
use crate::theme::{Hex, rgb, to_u32};

const SQUARE_W: f32 = 220.0;
const SQUARE_H: f32 = 150.0;
const HUE_H: f32 = 14.0;
const POPUP_W: f32 = 252.0;
const POPUP_H: f32 = 330.0;

/// Which part of the picker is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Square,
    Hue,
}

pub struct ColorPicker {
    /// Where to send the chosen colour (as "#RRGGBB").
    pub on: fn(String) -> Message,
    pub hue: f32,
    pub sat: f32,
    pub val: f32,
    pub hex: String,
    pub at: Point,
    pub dragging: Option<Area>,
    /// Pointer position inside each area, for presses.
    hover: [Point; 2],
}

/// Hue (0-360), saturation and value (0-1) to a colour.
pub fn hsv_to_color(h: f32, s: f32, v: f32) -> Color {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    Color::from_rgb(r + m, g + m, b + m)
}

pub fn color_to_hsv(c: Color) -> (f32, f32, f32) {
    let (r, g, b) = (c.r, c.g, c.b);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

fn hex_of(c: Color) -> String {
    format!("#{:06X}", to_u32(c))
}

impl ColorPicker {
    fn color(&self) -> Color {
        hsv_to_color(self.hue, self.sat, self.val)
    }

    fn apply(&mut self, area: Area, p: Point) {
        match area {
            Area::Square => {
                self.sat = (p.x / SQUARE_W).clamp(0.0, 1.0);
                self.val = 1.0 - (p.y / SQUARE_H).clamp(0.0, 1.0);
            }
            Area::Hue => self.hue = (p.x / SQUARE_W).clamp(0.0, 1.0) * 359.9,
        }
        self.hex = hex_of(self.color());
    }
}

impl App {
    pub(crate) fn update_color_picker(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenColorPicker(current, on) => {
                let (hue, sat, val) = color_to_hsv(rgb(current));
                self.color_picker = Some(ColorPicker {
                    on,
                    hue,
                    sat,
                    val,
                    hex: format!("#{current:06X}"),
                    at: crate::app_messages::last_cursor(),
                    dragging: None,
                    hover: [Point::ORIGIN; 2],
                });
            }
            Message::CloseColorPicker => self.color_picker = None,
            Message::ColorPickerPress(area) => {
                let Some(p) = &mut self.color_picker else { return Task::none() };
                p.dragging = Some(area);
                let at = p.hover[area as usize];
                p.apply(area, at);
                let (on, hex) = (p.on, p.hex.clone());
                return self.update(on(hex));
            }
            Message::ColorPickerMove(area, at) => {
                let Some(p) = &mut self.color_picker else { return Task::none() };
                p.hover[area as usize] = at;
                if p.dragging == Some(area) {
                    p.apply(area, at);
                    let (on, hex) = (p.on, p.hex.clone());
                    return self.update(on(hex));
                }
            }
            Message::ColorPickerRelease => {
                if let Some(p) = &mut self.color_picker {
                    p.dragging = None;
                }
            }
            Message::ColorPickerHex(hex) => {
                let Some(p) = &mut self.color_picker else { return Task::none() };
                if let Some(c) = Hex::parse(&hex) {
                    // Keep the hue when picking a grey, so the square doesn't jump to red.
                    let (h, s, v) = color_to_hsv(c);
                    if s > 0.0 {
                        p.hue = h;
                    }
                    p.sat = s;
                    p.val = v;
                }
                p.hex = hex.clone();
                let on = p.on;
                return self.update(on(hex));
            }
            _ => {}
        }
        Task::none()
    }

    /// The picker drawn over everything near where it was opened.
    pub(crate) fn with_color_picker<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        let Some(p) = &self.color_picker else { return base };
        let backdrop = mouse_area(container(text("")).width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseColorPicker);
        let at = p.at;
        let placed = responsive(move |size| {
            let x = (at.x + 12.0).min(size.width - POPUP_W - 8.0).max(0.0);
            let y = (at.y - 40.0).min(size.height - POPUP_H - 8.0).max(0.0);
            pin(self.color_picker_view(p)).x(x).y(y).into()
        });
        stack![base, backdrop, placed].into()
    }

    fn color_picker_view<'a>(&'a self, p: &'a ColorPicker) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let pure = hsv_to_color(p.hue, 1.0, 1.0);
        let fill = |g: gradient::Linear| {
            move |_: &iced::Theme| container::Style { background: Some(g.into()), ..Default::default() }
        };
        // Left to right: white to the pure hue; top to bottom: clear to black.
        let across =
            gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2)).add_stop(0.0, Color::WHITE).add_stop(1.0, pure);
        let down = gradient::Linear::new(Radians(std::f32::consts::PI))
            .add_stop(0.0, Color::TRANSPARENT)
            .add_stop(1.0, Color::BLACK);
        let marker = |size: f32| {
            container(text("")).width(size).height(size).style(move |_| container::Style {
                border: Border { radius: (size / 2.0).into(), width: 2.0, color: Color::WHITE },
                shadow: iced::Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.6),
                    blur_radius: 3.0,
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let square = mouse_area(stack![
            container(text("")).width(SQUARE_W).height(SQUARE_H).style(fill(across)),
            container(text("")).width(SQUARE_W).height(SQUARE_H).style(fill(down)),
            pin(marker(12.0)).x(p.sat * SQUARE_W - 6.0).y((1.0 - p.val) * SQUARE_H - 6.0),
        ])
        .on_move(|at| Message::ColorPickerMove(Area::Square, at))
        .on_press(Message::ColorPickerPress(Area::Square))
        .on_release(Message::ColorPickerRelease)
        .interaction(iced::mouse::Interaction::Crosshair);

        let mut rainbow = gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2));
        for i in 0..=6 {
            let f = i as f32 / 6.0;
            rainbow = rainbow.add_stop(f, hsv_to_color(f * 360.0, 1.0, 1.0));
        }
        let hue = mouse_area(stack![
            container(text("")).width(SQUARE_W).height(HUE_H).style(move |_| container::Style {
                background: Some(rainbow.into()),
                border: Border { radius: (HUE_H / 2.0).into(), ..Default::default() },
                ..Default::default()
            }),
            pin(marker(HUE_H)).x(p.hue / 360.0 * SQUARE_W - HUE_H / 2.0).y(0.0),
        ])
        .on_move(|at| Message::ColorPickerMove(Area::Hue, at))
        .on_press(Message::ColorPickerPress(Area::Hue))
        .on_release(Message::ColorPickerRelease)
        .interaction(iced::mouse::Interaction::Pointer);

        let current = p.color();
        let hex_row = row![
            container(text("")).width(32).height(32).style(card(current, t.radius * 0.5)),
            text_input("#RRGGBB", &p.hex)
                .on_input(Message::ColorPickerHex)
                .padding(8)
                .size(fs * 0.9)
                .style(style::input(t, t.rail.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        container(
            column![
                container(square).clip(true).style(card(Color::TRANSPARENT, 4.0)),
                hue,
                hex_row,
                style::swatches(to_u32(current), Message::ColorPickerHex),
            ]
            .spacing(12),
        )
        .padding(14)
        .width(POPUP_W)
        .style(card(t.floating.0, t.radius))
        .into()
    }
}

impl App {
    /// The colour square next to a colour field; click it to open the picker.
    pub(crate) fn color_swatch_button<'a>(
        &'a self,
        current: u32,
        radius: f32,
        on: fn(String) -> Message,
    ) -> Element<'a, Message> {
        let t = &self.theme;
        let border = t.hover.0;
        iced::widget::tooltip(
            iced::widget::button(container(text("")).width(36).height(36).style(card(rgb(current), radius)))
                .padding(0)
                .on_press(Message::OpenColorPicker(current, on))
                .style(move |_, status| iced::widget::button::Style {
                    border: Border {
                        radius: (radius + 2.0).into(),
                        width: if matches!(status, iced::widget::button::Status::Hovered) { 2.0 } else { 0.0 },
                        color: border,
                    },
                    ..Default::default()
                }),
            container(text("Pick a colour").size(self.fs() * 0.8)).padding([4, 8]).style(card(t.floating.0, 6.0)),
            iced::widget::tooltip::Position::Top,
        )
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_roundtrip() {
        for v in [0x5865F2, 0xF23F43, 0x23A55A, 0xFFFFFF, 0x000000, 0x808080, 0xEB459E] {
            let (h, s, val) = color_to_hsv(rgb(v));
            assert_eq!(to_u32(hsv_to_color(h, s, val)), v, "{v:06X}");
        }
        assert_eq!(hex_of(hsv_to_color(0.0, 1.0, 1.0)), "#FF0000");
        assert_eq!(hex_of(hsv_to_color(120.0, 1.0, 1.0)), "#00FF00");
    }
}
