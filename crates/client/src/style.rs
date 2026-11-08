//! Reusable styles and small widgets (avatars, profile cards, dividers).

use iced::widget::{button, column, container, row, scrollable, space, stack, text, text_input};
use iced::{Background, Border, Color, Element, Font, Length, Shadow, Theme, Vector, font};
use syncord_proto::{AvatarRing, Profile};

use crate::theme::{AppTheme, contrast_text, rgb, with_alpha};

pub const BOLD: Font = Font { weight: font::Weight::Bold, ..Font::DEFAULT };
pub const SEMIBOLD: Font = Font { weight: font::Weight::Semibold, ..Font::DEFAULT };

pub fn bg(c: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(c.into()), ..Default::default() }
}

pub fn card(c: Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(c.into()),
        border: Border { radius: radius.into(), ..Default::default() },
        shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.35), offset: Vector::new(0.0, 4.0), blur_radius: 16.0 },
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
    text(s.into().to_uppercase()).size(size * 0.8).font(BOLD).color(t.muted.0).into()
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
        container(text(glyph.clone()).size(inner * 0.44).font(BOLD).color(contrast_text(accent)))
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

/// Avatar with a small online/offline dot in the corner.
pub fn avatar_with_status<'a, M: 'a>(
    t: &AppTheme,
    name: &str,
    profile: &Profile,
    size: f32,
    online: bool,
    cutout: Color,
) -> Element<'a, M> {
    let dot_size = (size * 0.38).round();
    let color = if online { t.online.0 } else { t.muted.0 };
    let dot = container(space())
        .width(dot_size)
        .height(dot_size)
        .style(move |_| container::Style {
            background: Some(color.into()),
            border: Border { radius: (dot_size / 2.0).into(), width: dot_size * 0.22, color: cutout },
            ..Default::default()
        });
    stack![avatar(name, profile, size), container(dot).align_right(size).align_bottom(size)].into()
}

/// The pop-out profile card (also used as the live preview in settings).
pub fn profile_card<'a, M: 'a>(
    t: &AppTheme,
    username: &str,
    profile: &Profile,
    is_owner: bool,
    online: bool,
    fs: f32,
) -> Element<'a, M> {
    let banner_color = rgb(profile.banner_color);
    let radius = t.radius * 1.5;
    let banner = container(space()).width(Length::Fill).height(64).style(move |_| container::Style {
        background: Some(banner_color.into()),
        border: Border { radius: iced::border::top(radius), ..Default::default() },
        ..Default::default()
    });

    let display = if profile.display_name.trim().is_empty() { username } else { profile.display_name.as_str() };

    let mut body = column![
        avatar_with_status(t, display, profile, 80.0, online, t.floating.0),
        column![
            text(display.to_owned()).size(fs * 1.35).font(BOLD).color(t.header.0),
            text(username.to_owned()).size(fs * 0.9).color(t.muted.0),
        ]
        .spacing(2),
    ]
    .spacing(10);

    if is_owner {
        body = body.push(
            container(text("Server Owner").size(fs * 0.75).font(BOLD).color(contrast_text(t.accent.0)))
                .padding([2, 8])
                .style(card(t.accent.0, 10.0)),
        );
    }
    if !profile.status_text.trim().is_empty() {
        body = body.push(text(profile.status_text.clone()).size(fs * 0.95).color(t.text.0));
    }
    body = body.push(divider(t.hover.0));
    body = body.push(label(t, "About me", fs));
    let bio = if profile.bio.trim().is_empty() { "No bio yet.".to_string() } else { profile.bio.clone() };
    body = body.push(text(bio).size(fs * 0.95).color(t.text.0));

    container(column![banner, container(body).padding([12, 16])])
        .width(320)
        .style(card(t.floating.0, radius))
        .into()
}

/// A horizontal row of colour swatches that set a colour field.
pub fn swatches<'a, M: Clone + 'a>(selected: u32, on_pick: impl Fn(String) -> M) -> Element<'a, M> {
    const COLORS: [u32; 12] = [
        0x5865F2, 0x3BA55C, 0xFAA61A, 0xED4245, 0xEB459E, 0x1ABC9C, 0x9B59B6, 0xE67E22, 0x3498DB, 0x95A5A6,
        0xFFFFFF, 0x23272A,
    ];
    let mut r = row![].spacing(6);
    for c in COLORS {
        let color = rgb(c);
        let is_sel = c == selected;
        r = r.push(
            button(space().width(22).height(22))
                .padding(0)
                .on_press(on_pick(format!("#{c:06X}")))
                .style(move |_, status| button::Style {
                    background: Some(color.into()),
                    border: Border {
                        radius: 6.0.into(),
                        width: if is_sel || matches!(status, button::Status::Hovered) { 2.0 } else { 0.0 },
                        color: contrast_text(color),
                    },
                    ..Default::default()
                }),
        );
    }
    r.wrap().into()
}
