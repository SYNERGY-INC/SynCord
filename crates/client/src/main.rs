//! SynCord desktop client.

// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod app_media;
mod app_social;
mod emoji;
mod icons;
mod media;
mod net;
mod settings;
mod style;
mod theme;
mod tls;
mod updater;
mod view;
mod view_extras;
mod voice;

use app::App;

fn main() -> iced::Result {
    // HTTPS (for servers behind a proxy like Caddy) uses the same small crypto library as the WebSocket.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Pick the renderer before iced starts. An explicit ICED_BACKEND wins.
    if std::env::var_os("ICED_BACKEND").is_none() && settings::Settings::load().low_memory {
        // SAFETY: nothing else is running yet, so no other thread can be reading the environment.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    }

    iced::application(App::new, App::update, App::view)
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
        .default_font(style::INTER)
        .run()
}
