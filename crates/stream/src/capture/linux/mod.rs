//! Linux: V4L2 cameras, X11 screens.

mod camera;
mod screen;

pub(crate) use camera::{list_cameras, run_camera};
pub(crate) use screen::{list_screens, run_screen};
