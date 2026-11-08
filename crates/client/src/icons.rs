//! Small single-colour SVG icons, tinted to match the theme.

use std::sync::LazyLock;

use iced::widget::svg::{self, Handle};
use iced::{Color, Element};

macro_rules! icon {
    ($name:ident, $body:expr) => {
        pub static $name: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(
                concat!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">"#, $body, "</svg>").as_bytes(),
            )
        });
    };
}

// `concat!` only accepts literals, so shared shapes are macros.
macro_rules! mic {
    () => {
        concat!(
            r#"<rect x="9" y="2.5" width="6" height="11.5" rx="3" fill="black"/>"#,
            r#"<path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5v3.5M8.5 21h7" stroke="black" stroke-width="2" fill="none" stroke-linecap="round"/>"#
        )
    };
}
macro_rules! headphones {
    () => {
        concat!(
            r#"<path d="M4 15v-3a8 8 0 0 1 16 0v3" stroke="black" stroke-width="2.2" fill="none"/>"#,
            r#"<rect x="2.5" y="13" width="5.5" height="8.5" rx="2" fill="black"/>"#,
            r#"<rect x="16" y="13" width="5.5" height="8.5" rx="2" fill="black"/>"#
        )
    };
}
macro_rules! slash {
    () => {
        r#"<path d="M3.5 3.5l17 17" stroke="black" stroke-width="2.6" stroke-linecap="round"/>"#
    };
}

icon!(
    SPEAKER,
    concat!(
        r#"<path d="M3 9h4l5.5-4.5v15L7 15H3z" fill="black"/>"#,
        r#"<path d="M15.5 8.5a5 5 0 0 1 0 7M18 6a8.5 8.5 0 0 1 0 12" stroke="black" stroke-width="2" fill="none" stroke-linecap="round"/>"#
    )
);
icon!(MIC_ON, mic!());
icon!(MIC_OFF, concat!(mic!(), slash!()));
icon!(HEADPHONES_ON, headphones!());
icon!(HEADPHONES_OFF, concat!(headphones!(), slash!()));
icon!(
    GEAR,
    concat!(
        r#"<circle cx="12" cy="12" r="7.6" fill="none" stroke="black" stroke-width="3.4" stroke-dasharray="3 2.97"/>"#,
        r#"<path fill-rule="evenodd" fill="black" d="M12 5.4a6.6 6.6 0 1 0 0.001 0zM12 9.3a2.7 2.7 0 1 1 -0.001 0z"/>"#
    )
);
icon!(
    HANG_UP,
    r#"<path d="M2.5 13.5c5.2-5 13.8-5 19 0l-2.6 3.2-3.4-1.4v-2.7c-2.3-.8-5.7-.8-7 0v2.7l-3.4 1.4z" fill="black"/>"#
);
icon!(
    SIGNAL,
    concat!(
        r#"<rect x="3" y="15" width="4" height="6" rx="1" fill="black"/>"#,
        r#"<rect x="10" y="10" width="4" height="11" rx="1" fill="black"/>"#,
        r#"<rect x="17" y="4" width="4" height="17" rx="1" fill="black"/>"#
    )
);

pub fn icon<'a, M: 'a>(handle: &LazyLock<Handle>, size: f32, color: Color) -> Element<'a, M> {
    iced::widget::svg((**handle).clone())
        .width(size)
        .height(size)
        .style(move |_, _| svg::Style { color: Some(color) })
        .into()
}
