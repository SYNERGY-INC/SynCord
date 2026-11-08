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

icon!(
    ATTACH,
    concat!(
        r#"<circle cx="12" cy="12" r="9.5" fill="none" stroke="black" stroke-width="2.2"/>"#,
        r#"<path d="M12 7.5v9M7.5 12h9" stroke="black" stroke-width="2.4" stroke-linecap="round"/>"#
    )
);
icon!(
    SMILE,
    concat!(
        r#"<circle cx="12" cy="12" r="9.5" fill="none" stroke="black" stroke-width="2"/>"#,
        r#"<circle cx="8.8" cy="10" r="1.4" fill="black"/><circle cx="15.2" cy="10" r="1.4" fill="black"/>"#,
        r#"<path d="M7.8 14.2a5 5 0 0 0 8.4 0" stroke="black" stroke-width="2" fill="none" stroke-linecap="round"/>"#
    )
);
icon!(
    DIRECT,
    concat!(
        r#"<path d="M4 4.5h11a2.5 2.5 0 0 1 2.5 2.5v6a2.5 2.5 0 0 1-2.5 2.5H9l-4 3v-3H4A2.5 2.5 0 0 1 1.5 13V7A2.5 2.5 0 0 1 4 4.5z" fill="black"/>"#,
        r#"<path d="M19.5 9h.5a2.5 2.5 0 0 1 2.5 2.5v5a2.5 2.5 0 0 1-2.5 2.5h-1v2.5l-3.4-2.5H11" stroke="black" stroke-width="1.8" fill="none" stroke-linejoin="round"/>"#
    )
);
icon!(
    STAR,
    r#"<path d="M12 2.8l2.8 5.9 6.4.8-4.7 4.4 1.2 6.4L12 17.2l-5.7 3.1 1.2-6.4-4.7-4.4 6.4-.8z" fill="black"/>"#
);
icon!(
    STAR_OUTLINE,
    r#"<path d="M12 3.8l2.5 5.2 5.7.7-4.2 3.9 1.1 5.7L12 16.5l-5.1 2.8 1.1-5.7-4.2-3.9 5.7-.7z" fill="none" stroke="black" stroke-width="1.8" stroke-linejoin="round"/>"#
);
icon!(
    DOWNLOAD,
    concat!(
        r#"<path d="M12 3.5v11M7 10l5 5 5-5" stroke="black" stroke-width="2.2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M4 17v2.5h16V17" stroke="black" stroke-width="2.2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#
    )
);
icon!(
    FILE,
    concat!(
        r#"<path d="M6 2.5h8l5 5v14H6z" fill="none" stroke="black" stroke-width="1.8" stroke-linejoin="round"/>"#,
        r#"<path d="M14 2.5v5h5M8.5 12.5h8M8.5 16h8" stroke="black" stroke-width="1.8" fill="none"/>"#
    )
);
icon!(
    MORE,
    r#"<circle cx="5" cy="12" r="2.2" fill="black"/><circle cx="12" cy="12" r="2.2" fill="black"/><circle cx="19" cy="12" r="2.2" fill="black"/>"#
);
icon!(CLOSE, r#"<path d="M6 6l12 12M18 6L6 18" stroke="black" stroke-width="2.4" stroke-linecap="round"/>"#);

pub fn icon<'a, M: 'a>(handle: &LazyLock<Handle>, size: f32, color: Color) -> Element<'a, M> {
    iced::widget::svg((**handle).clone())
        .width(size)
        .height(size)
        .style(move |_, _| svg::Style { color: Some(color) })
        .into()
}
