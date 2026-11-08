//! SynCord desktop client.

// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod app_avatars;
mod app_media;
mod app_messages;
mod app_people;
mod app_social;
mod composer;
mod crop;
mod emoji;
mod fonts;
mod icons;
mod media;
mod net;
mod settings;
mod shortcuts;
mod sound;
mod sounds;
mod style;
mod theme;
mod theme_editor;
mod tls;
mod updater;
mod video;
mod view;
mod view_extras;
mod view_menus;
mod view_messages;
mod view_people;
mod view_roles;
mod voice;
mod voice_moderation;
mod widgets;

use app::App;

fn main() -> iced::Result {
    // HTTPS (for servers behind a proxy like Caddy) uses the same small crypto library as the WebSocket.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Videos downloaded to watch last time aren't needed any more.
    video::clear_cache();
    // Pick the renderer before iced starts. An explicit ICED_BACKEND wins.
    let saved = settings::Settings::load();
    if std::env::var_os("ICED_BACKEND").is_none() && saved.low_memory {
        // SAFETY: nothing else is running yet, so no other thread can be reading the environment.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    }

    // The UI font is chosen once per run (see Appearance > Font).
    let (ui_font, font_file) = fonts::init(&saved);

    let mut app = iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::iced_theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            min_size: Some(iced::Size::new(940.0, 560.0)),
            icon: iced::window::icon::from_rgba(
                include_bytes!("../../../packaging/icons/syncord-64.rgba").to_vec(),
                64,
                64,
            )
            .ok(),
            // Lets Linux desktops match the window to syncord.desktop (and its icon).
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "syncord".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .antialiasing(true)
        .font(include_bytes!("../assets/fonts/Inter-Regular.otf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-SemiBold.otf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-Bold.otf").as_slice())
        // The same emoji Discord uses (Twemoji), so they look the same everywhere.
        .font(include_bytes!("../assets/fonts/Twemoji.ttf").as_slice())
        .default_font(ui_font);
    if let Some(bytes) = font_file {
        app = app.font(bytes);
    }
    app.run()
}
