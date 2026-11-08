//! Reusable styles and small widgets (avatars, profile cards, dividers).

use iced::widget::{button, container, row, scrollable, space, text, text_input};
use iced::{Background, Border, Color, Element, Font, Length, Theme, font};
use syncord_proto::{AvatarRing, Profile};

use crate::theme::{AppTheme, contrast_text, rgb, with_alpha};

/// The bundled UI font (see `assets/fonts`), so text looks the same on every computer.
pub const INTER: Font = Font::with_name("Inter");

/// The UI font chosen in Appearance settings (Inter unless changed), in bold.
pub fn bold() -> Font {
    Font { weight: font::Weight::Bold, ..crate::fonts::ui() }
}

/// The UI font chosen in Appearance settings, semibold. Many installed fonts have no
/// semibold weight (text would fall back to another font), so those use bold.
pub fn semibold() -> Font {
    let ui = crate::fonts::ui();
    let weight = if ui == INTER { font::Weight::Semibold } else { font::Weight::Bold };
    Font { weight, ..ui }
}

pub fn bg(c: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(c.into()), ..Default::default() }
}

/// Rounded panel. Deliberately shadow-free: shadows bleed outside a widget's
/// bounds, which leaves smears with the CPU renderer's partial redraws.
pub fn card(c: Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    flat(c, radius)
}

/// Rounded block without a shadow.
pub fn flat(c: Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(c.into()),
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn divider<'a, M: 'a>(c: Color) -> Element<'a, M> {
    container(space()).width(Length::Fill).height(1).style(bg(c)).into()
}

/// A flat button that only shows a background when hovered or selected (channel rows, member rows).
pub fn row_button(t: &AppTheme, selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (hover, sel, text, header, radius) = (t.hover.0, t.selected.0, t.muted.0, t.header.0, t.radius * 0.6);
    move |_, status| {
        let background = match (selected, status) {
            (true, _) => Some(sel.into()),
            (false, button::Status::Hovered | button::Status::Pressed) => Some(hover.into()),
            _ => None,
        };
        let text_color = if selected || matches!(status, button::Status::Hovered) { header } else { text };
        button::Style {
            background,
            text_color,
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

/// A list row whose hover background fades (see [`fade_row`]): only the selected
/// state is drawn here.
pub fn row_button_still(t: &AppTheme, selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + use<> {
    let (sel, text, header, radius) = (t.selected.0, t.muted.0, t.header.0, t.radius * 0.6);
    move |_, status| button::Style {
        background: selected.then(|| sel.into()),
        text_color: if selected || matches!(status, button::Status::Hovered) { header } else { text },
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    }
}

/// A list row (channel, person, conversation) with a hover highlight that fades in and out.
pub fn fade_row<'a, M: Clone + 'a>(t: &AppTheme, row: iced::widget::Button<'a, M>, selected: bool) -> Element<'a, M> {
    crate::widgets::hover_fade(row.style(row_button_still(t, selected)), t.hover.0, t.radius * 0.6).into()
}

/// Solid button in the given colour.
pub fn solid_button(color: Color, radius: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let c = match status {
            button::Status::Hovered => darken(color, 0.1),
            button::Status::Pressed => darken(color, 0.2),
            button::Status::Disabled => with_alpha(color, 0.5),
            button::Status::Active => color,
        };
        button::Style {
            background: Some(c.into()),
            text_color: contrast_text(color),
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

/// Text-only button (links, small icon buttons).
pub fn link_button(color: Color, hover: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered) { hover } else { color },
        ..Default::default()
    }
}

/// Small square icon button with a hover background.
pub fn icon_button(t: &AppTheme) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (muted, header, hover, radius) = (t.muted.0, t.header.0, t.hover.0, t.radius * 0.5);
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: hovered.then(|| hover.into()),
            text_color: if hovered { header } else { muted },
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

pub fn input(t: &AppTheme, fill: Color) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    let (muted, text, accent, radius) = (t.muted.0, t.text.0, t.accent.0, t.radius);
    move |_, status| text_input::Style {
        background: Background::Color(fill),
        border: Border {
            radius: radius.into(),
            width: 1.0,
            color: if matches!(status, text_input::Status::Focused { .. }) { accent } else { Color::TRANSPARENT },
        },
        icon: muted,
        placeholder: muted,
        value: text,
        selection: with_alpha(accent, 0.4),
    }
}

pub fn scroll(t: &AppTheme) -> impl Fn(&Theme, scrollable::Status) -> scrollable::Style {
    let thumb = with_alpha(t.muted.0, 0.35);
    move |theme, status| {
        let mut s = scrollable::default(theme, status);
        for rail in [&mut s.vertical_rail, &mut s.horizontal_rail] {
            rail.background = None;
            rail.border = Border::default();
            rail.scroller.background = thumb.into();
            rail.scroller.border = Border { radius: 4.0.into(), ..Default::default() };
        }
        s
    }
}

pub fn darken(c: Color, amount: f32) -> Color {
    Color { r: c.r * (1.0 - amount), g: c.g * (1.0 - amount), b: c.b * (1.0 - amount), a: c.a }
}

/// Uppercase section label like "TEXT CHANNELS".
pub fn label<'a, M: 'a>(t: &AppTheme, s: impl Into<String>, size: f32) -> Element<'a, M> {
    text(s.into().to_uppercase()).size(size * 0.8).font(bold()).color(t.muted.0).into()
}

/// The circular avatar, including the user's chosen ring decoration.
pub fn avatar<'a, M: 'a>(name: &str, profile: &Profile, size: f32) -> Element<'a, M> {
    let accent = rgb(profile.accent_color);
    let ring = rgb(profile.ring_color);
    let glyph = if profile.avatar_glyph.trim().is_empty() {
        name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
    } else {
        profile.avatar_glyph.clone()
    };

    let core = |inner: f32, border: Border| -> Element<'a, M> {
        container(text(glyph.clone()).size(inner * 0.44).font(bold()).color(contrast_text(accent)))
            .center(inner)
            .style(move |_| container::Style {
                background: Some(accent.into()),
                border: Border { radius: (inner / 2.0).into(), ..border },
                ..Default::default()
            })
            .into()
    };
    let ring_border = |w: f32| Border { width: w, color: ring, radius: 0.0.into() };

    match profile.avatar_ring {
        AvatarRing::None => core(size, Border::default()),
        AvatarRing::Thin => core(size, ring_border((size * 0.05).max(1.5))),
        AvatarRing::Thick => core(size, ring_border((size * 0.1).max(2.5))),
        AvatarRing::Double => {
            let w = (size * 0.045).max(1.5);
            let inner = size - 4.0 * w;
            container(core(inner, ring_border(w)))
                .center(size)
                .style(move |_| container::Style {
                    border: Border { width: w, color: ring, radius: (size / 2.0).into() },
                    ..Default::default()
                })
                .into()
        }
    }
}

/// A horizontal row of colour swatches that set a colour field.
pub fn swatches<'a, M: Clone + 'a>(selected: u32, on_pick: impl Fn(String) -> M) -> Element<'a, M> {
    const COLORS: [u32; 12] = [
        0x5865F2, 0x3BA55C, 0xFAA61A, 0xED4245, 0xEB459E, 0x1ABC9C, 0x9B59B6, 0xE67E22, 0x3498DB, 0x95A5A6, 0xFFFFFF,
        0x23272A,
    ];
    let mut r = row![].spacing(6);
    for c in COLORS {
        let color = rgb(c);
        let is_sel = c == selected;
        r = r.push(button(space().width(22).height(22)).padding(0).on_press(on_pick(format!("#{c:06X}"))).style(
            move |_, status| button::Style {
                background: Some(color.into()),
                border: Border {
                    radius: 6.0.into(),
                    width: if is_sel || matches!(status, button::Status::Hovered) { 2.0 } else { 0.0 },
                    color: contrast_text(color),
                },
                ..Default::default()
            },
        ));
    }
    r.wrap().into()
}

/// Colour emoji: Twemoji (the set Discord uses), bundled with the app.
pub const EMOJI_FONT: Font = Font::with_name("Twemoji Mozilla");

fn is_emoji_char(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF // pictographs, faces, flags, symbols
        | 0x2600..=0x27BF // misc symbols and dingbats
        | 0x2B00..=0x2BFF // stars, arrows used as emoji
        | 0x231A..=0x231B | 0x23E9..=0x23FA // watch, hourglass, media keys
        | 0xFE0F | 0x200D | 0x20E3 // emoji presentation, joiner, keycap
        | 0xE0020..=0xE007F // tag sequences (subdivision flags)
        | 0x3030 | 0x303D | 0x3297 | 0x3299)
}

/// Split text into runs of plain text and emoji.
pub fn split_emoji(s: &str) -> Vec<(bool, &str)> {
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let e = is_emoji_char(c);
        if current.is_some_and(|cur| cur != e) {
            out.push((current.unwrap(), &s[start..i]));
            start = i;
        }
        current = Some(e);
    }
    if let Some(cur) = current {
        out.push((cur, &s[start..]));
    }
    out
}

/// An emoji (or a few) drawn with the colour emoji font.
pub fn emoji_text<'a>(e: impl text::IntoFragment<'a>, size: f32) -> iced::widget::Text<'a> {
    text(e).font(EMOJI_FONT).size(size)
}

#[cfg(test)]
mod tests {
    use super::split_emoji;

    #[test]
    fn splits_emoji_runs() {
        assert_eq!(split_emoji("hi 👋 there"), vec![(false, "hi "), (true, "👋"), (false, " there")]);
        assert_eq!(split_emoji("🔥🔥"), vec![(true, "🔥🔥")]);
        assert_eq!(split_emoji("❤️ ok"), vec![(true, "❤️"), (false, " ok")]);
        assert_eq!(split_emoji("👨‍👩‍👧"), vec![(true, "👨‍👩‍👧")]);
        assert_eq!(split_emoji(""), vec![]);
    }
}
