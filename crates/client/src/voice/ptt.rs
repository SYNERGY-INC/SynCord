//! Push to talk: is the chosen key (or mouse side button) held down right now, even while
//! another program is in front (a game)?
//!
//! Asked about every 20 ms by the capture thread. Windows (GetAsyncKeyState), macOS
//! (CGEventSourceKeyState; needs Input Monitoring permission) and X11 (QueryKeymap) can
//! all tell without hooks. Where they can't (Wayland), the key works while Celerity is in
//! front, from the window's own key events.

use std::sync::atomic::{AtomicBool, Ordering};

/// Held according to Celerity's own window (works everywhere, but only while focused).
static IN_WINDOW: AtomicBool = AtomicBool::new(false);

pub fn set_in_window(down: bool) {
    IN_WINDOW.store(down, Ordering::Relaxed);
}

/// Keys are saved by their position name (`KeyV`, `F13`, `Backquote`...), like iced's
/// `key::Code`, plus `Mouse4` / `Mouse5` for the side buttons.
pub fn is_down(key: &str) -> bool {
    IN_WINDOW.load(Ordering::Relaxed) || imp::is_down(key)
}

/// How a key name reads in Settings ("KeyV" -> "V").
pub fn label(key: &str) -> String {
    match key {
        "" => "Not set".into(),
        "Mouse4" => "Mouse button 4".into(),
        "Mouse5" => "Mouse button 5".into(),
        k if k.starts_with("Key") && k.len() == 4 => k[3..].into(),
        k if k.starts_with("Digit") && k.len() == 6 => k[5..].into(),
        "Backquote" => "`".into(),
        "CapsLock" => "Caps Lock".into(),
        "ControlLeft" => "Left Ctrl".into(),
        "ControlRight" => "Right Ctrl".into(),
        "ShiftLeft" => "Left Shift".into(),
        "ShiftRight" => "Right Shift".into(),
        "AltLeft" => "Left Alt".into(),
        "AltRight" => "Right Alt".into(),
        k => k.replace("Numpad", "Numpad "),
    }
}

/// Letters, digits, F-keys and the usual push-to-talk keys, with their codes on each system:
/// (name, Windows virtual key, macOS key code, Linux evdev code).
const KEYS: &[(&str, u16, u16, u16)] = &[
    ("KeyA", 0x41, 0x00, 30),
    ("KeyB", 0x42, 0x0B, 48),
    ("KeyC", 0x43, 0x08, 46),
    ("KeyD", 0x44, 0x02, 32),
    ("KeyE", 0x45, 0x0E, 18),
    ("KeyF", 0x46, 0x03, 33),
    ("KeyG", 0x47, 0x05, 34),
    ("KeyH", 0x48, 0x04, 35),
    ("KeyI", 0x49, 0x22, 23),
    ("KeyJ", 0x4A, 0x26, 36),
    ("KeyK", 0x4B, 0x28, 37),
    ("KeyL", 0x4C, 0x25, 38),
    ("KeyM", 0x4D, 0x2E, 50),
    ("KeyN", 0x4E, 0x2D, 49),
    ("KeyO", 0x4F, 0x1F, 24),
    ("KeyP", 0x50, 0x23, 25),
    ("KeyQ", 0x51, 0x0C, 16),
    ("KeyR", 0x52, 0x0F, 19),
    ("KeyS", 0x53, 0x01, 31),
    ("KeyT", 0x54, 0x11, 20),
    ("KeyU", 0x55, 0x20, 22),
    ("KeyV", 0x56, 0x09, 47),
    ("KeyW", 0x57, 0x0D, 17),
    ("KeyX", 0x58, 0x07, 45),
    ("KeyY", 0x59, 0x10, 21),
    ("KeyZ", 0x5A, 0x06, 44),
    ("Digit0", 0x30, 0x1D, 11),
    ("Digit1", 0x31, 0x12, 2),
    ("Digit2", 0x32, 0x13, 3),
    ("Digit3", 0x33, 0x14, 4),
    ("Digit4", 0x34, 0x15, 5),
    ("Digit5", 0x35, 0x17, 6),
    ("Digit6", 0x36, 0x16, 7),
    ("Digit7", 0x37, 0x1A, 8),
    ("Digit8", 0x38, 0x1C, 9),
    ("Digit9", 0x39, 0x19, 10),
    ("F1", 0x70, 0x7A, 59),
    ("F2", 0x71, 0x78, 60),
    ("F3", 0x72, 0x63, 61),
    ("F4", 0x73, 0x76, 62),
    ("F5", 0x74, 0x60, 63),
    ("F6", 0x75, 0x61, 64),
    ("F7", 0x76, 0x62, 65),
    ("F8", 0x77, 0x64, 66),
    ("F9", 0x78, 0x65, 67),
    ("F10", 0x79, 0x6D, 68),
    ("F11", 0x7A, 0x67, 87),
    ("F12", 0x7B, 0x6F, 88),
    ("F13", 0x7C, 0x69, 183),
    ("F14", 0x7D, 0x6B, 184),
    ("F15", 0x7E, 0x71, 185),
    ("Backquote", 0xC0, 0x32, 41),
    ("Space", 0x20, 0x31, 57),
    ("Tab", 0x09, 0x30, 15),
    ("CapsLock", 0x14, 0x39, 58),
    ("ShiftLeft", 0xA0, 0x38, 42),
    ("ShiftRight", 0xA1, 0x3C, 54),
    ("ControlLeft", 0xA2, 0x3B, 29),
    ("ControlRight", 0xA3, 0x3E, 97),
    ("AltLeft", 0xA4, 0x3A, 56),
    ("AltRight", 0xA5, 0x3D, 100),
    ("Insert", 0x2D, 0x72, 110),
    ("Delete", 0x2E, 0x75, 111),
    ("Home", 0x24, 0x73, 102),
    ("End", 0x23, 0x77, 107),
    ("PageUp", 0x21, 0x74, 104),
    ("PageDown", 0x22, 0x79, 109),
    ("Numpad0", 0x60, 0x52, 82),
    ("Numpad1", 0x61, 0x53, 79),
    ("Numpad2", 0x62, 0x54, 80),
    ("Numpad3", 0x63, 0x55, 81),
    ("Numpad4", 0x64, 0x56, 75),
    ("Numpad5", 0x65, 0x57, 76),
    ("Numpad6", 0x66, 0x58, 77),
    ("Numpad7", 0x67, 0x59, 71),
    ("Numpad8", 0x68, 0x5B, 72),
    ("Numpad9", 0x69, 0x5C, 73),
];

/// Keys that can be chosen (anything else is refused, so it surely works everywhere).
pub fn supported(key: &str) -> bool {
    key == "Mouse4" || key == "Mouse5" || KEYS.iter().any(|k| k.0 == key)
}

#[allow(dead_code)]
fn codes(key: &str) -> Option<(u16, u16, u16)> {
    KEYS.iter().find(|k| k.0 == key).map(|k| (k.1, k.2, k.3))
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_XBUTTON1, VK_XBUTTON2};

    pub fn is_down(key: &str) -> bool {
        let vk = match key {
            "Mouse4" => VK_XBUTTON1,
            "Mouse5" => VK_XBUTTON2,
            k => match super::codes(k) {
                Some((vk, _, _)) => vk,
                None => return false,
            },
        };
        // SAFETY: a plain query of the keyboard state.
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
    }
}

#[cfg(target_os = "macos")]
mod imp {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    }
    /// kCGEventSourceStateHIDSystemState: the hardware, whichever app is in front.
    const HID: i32 = 1;

    pub fn is_down(key: &str) -> bool {
        // SAFETY: plain queries of the input state.
        unsafe {
            match key {
                "Mouse4" => CGEventSourceButtonState(HID, 3),
                "Mouse5" => CGEventSourceButtonState(HID, 4),
                k => super::codes(k).is_some_and(|(_, mac, _)| CGEventSourceKeyState(HID, mac)),
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::cell::RefCell;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    use x11rb::rust_connection::RustConnection;

    thread_local! {
        /// One connection per thread that asks (the capture thread), opened on first use.
        static X: RefCell<Option<Option<RustConnection>>> = const { RefCell::new(None) };
    }

    pub fn is_down(key: &str) -> bool {
        let Some((_, _, evdev)) = super::codes(key) else { return false };
        X.with(|x| {
            let mut x = x.borrow_mut();
            let conn = x.get_or_insert_with(|| x11rb::connect(None).ok().map(|(c, _)| c));
            let Some(conn) = conn.as_ref() else { return false };
            let Ok(reply) = conn.query_keymap().map_err(drop).and_then(|c| c.reply().map_err(drop)) else {
                return false;
            };
            // X key codes are evdev codes + 8.
            let code = evdev as usize + 8;
            let _ = conn.flush();
            reply.keys.get(code / 8).is_some_and(|byte| byte & (1 << (code % 8)) != 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(label("KeyV"), "V");
        assert_eq!(label("Digit3"), "3");
        assert_eq!(label("Numpad5"), "Numpad 5");
        assert!(supported("F13") && supported("Mouse4") && !supported("Fn"));
        // Every key has distinct codes on each system.
        for (i, a) in KEYS.iter().enumerate() {
            for b in &KEYS[i + 1..] {
                assert!(a.1 != b.1 && a.2 != b.2 && a.3 != b.3, "{} and {} clash", a.0, b.0);
            }
        }
    }
}
