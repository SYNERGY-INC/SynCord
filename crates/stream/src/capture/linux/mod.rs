//! Linux: V4L2 cameras, X11 screens and windows.

mod camera;
mod screen;
mod window;

pub(crate) use camera::{list_cameras, run_camera};
pub(crate) use screen::{list_screens, run_screen};
