//! SynCord desktop client.

// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod net;
mod settings;
mod style;
mod theme;
mod view;

use app::App;

fn main() -> iced::Result {
    // Pick the renderer before iced starts. An explicit ICED_BACKEND wins.
    if std::env::var_os("ICED_BACKEND").is_none() && settings::Settings::load().low_memory {
        // SAFETY: nothing else is running yet, so no other thread can be reading the environment.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    }

    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::iced_theme)
        .subscription(App::subscription)
        .window_size((1280.0, 800.0))
        .antialiasing(true)
        .run()
}
