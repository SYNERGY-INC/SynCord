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
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::iced_theme)
        .subscription(App::subscription)
        .window_size((1280.0, 800.0))
        .antialiasing(true)
        .run()
}
