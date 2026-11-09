//! Celerity desktop client.

// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod app_avatars;
mod app_channels;
mod app_expressions;
mod app_groups;
mod app_invites;
mod app_media;
mod app_messages;
mod app_people;
mod app_search;
mod app_social;
mod app_threads;
mod color_picker;
mod composer;
mod crop;
mod e2ee;
mod emoji;
mod features;
mod fonts;
mod icons;
mod live;
mod markdown;
mod media;
mod net;
mod notify;
mod polls;
mod presence;
mod press;
mod servers;
mod settings;
mod shortcuts;
mod sound;
mod soundboard;
mod sounds;
mod stream_audio;
mod style;
mod theme;
mod theme_editor;
mod tls;
mod tray;
mod updater;
mod video;
mod video_tile;
mod view;
mod view_channels;
mod view_embeds;
mod view_expressions;
mod view_extras;
mod view_groups;
mod view_live;
mod view_menus;
mod view_messages;
mod view_people;
mod view_roles;
mod view_threads;
mod voice;
mod voice_moderation;
mod widgets;

use app::App;

fn main() -> iced::Result {
    // HTTPS (for servers behind a proxy like Caddy) uses the same small crypto library as the WebSocket.
    let _ = rustls::crypto::ring::default_provider().install_default();
    settings::adopt_legacy_folder();
    // Windows: the name and icon notifications show, and celerity:// links.
    notify::register();
    // Already running (maybe in the tray)? Bring that copy to the front instead.
    if !tray::claim_single_instance() {
        return Ok(());
    }
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
        // The X button may hide Celerity to the tray instead (see App::update).
        .exit_on_close_request(false)
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            min_size: Some(iced::Size::new(940.0, 560.0)),
            icon: iced::window::icon::from_rgba(
                include_bytes!("../../../packaging/icons/celerity-64.rgba").to_vec(),
                64,
                64,
            )
            .ok(),
            // Lets Linux desktops match the window to celerity.desktop (and its icon).
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "celerity".into(),
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
