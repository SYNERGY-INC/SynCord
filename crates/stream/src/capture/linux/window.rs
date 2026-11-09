//! X11 window capture with XComposite: the window is redirected (automatic mode, so it still
//! shows normally on screen) and its off-screen pixmap read with MIT-SHM. This shows the
//! window's own contents even when other windows cover it. Windows come from the window
//! manager's `_NET_CLIENT_LIST`, or the root's mapped children without one.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use super::screen::{Pointer, ShmSeg, connect, has_xfixes};
use crate::capture::{Pacer, Ready, ScreenConfig, ScreenInfo, ScreenKind, Sink, Stop};
use crate::convert::{Converter, RawFrame};
use crate::frame::I420Frame;
use crate::now_us;

fn atom(conn: &RustConnection, name: &str) -> Option<Atom> {
    conn.intern_atom(true, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom).filter(|&a| a != 0)
}

/// A whole property as bytes (`None` if missing).
fn property(conn: &RustConnection, win: Window, prop: Atom, ty: impl Into<Atom>) -> Option<Vec<u8>> {
    let r = conn.get_property(false, win, prop, ty.into(), 0, 1 << 16).ok()?.reply().ok()?;
    (r.format != 0).then_some(r.value)
}

fn property32(conn: &RustConnection, win: Window, prop: Atom, ty: impl Into<Atom>) -> Vec<u32> {
    property(conn, win, prop, ty)
        .map(|v| v.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect())
        .unwrap_or_default()
}

struct Atoms {
    client_list: Option<Atom>,
    net_name: Option<Atom>,
    utf8: Option<Atom>,
    pid: Option<Atom>,
    wm_type: Option<Atom>,
    /// Window types that are not application windows (docks, desktop, menus...).
    skip_types: Vec<Atom>,
}

impl Atoms {
    fn get(conn: &RustConnection) -> Atoms {
        let skip = [
            "_NET_WM_WINDOW_TYPE_DESKTOP",
            "_NET_WM_WINDOW_TYPE_DOCK",
            "_NET_WM_WINDOW_TYPE_TOOLBAR",
            "_NET_WM_WINDOW_TYPE_MENU",
            "_NET_WM_WINDOW_TYPE_SPLASH",
            "_NET_WM_WINDOW_TYPE_NOTIFICATION",
            "_NET_WM_WINDOW_TYPE_TOOLTIP",
            "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
            "_NET_WM_WINDOW_TYPE_POPUP_MENU",
            "_NET_WM_WINDOW_TYPE_DND",
        ];
        Atoms {
            client_list: atom(conn, "_NET_CLIENT_LIST"),
            net_name: atom(conn, "_NET_WM_NAME"),
            utf8: atom(conn, "UTF8_STRING"),
            pid: atom(conn, "_NET_WM_PID"),
            wm_type: atom(conn, "_NET_WM_WINDOW_TYPE"),
            skip_types: skip.iter().filter_map(|n| atom(conn, n)).collect(),
        }
    }
}

fn title(conn: &RustConnection, a: &Atoms, win: Window) -> String {
    if let (Some(name), Some(utf8)) = (a.net_name, a.utf8)
        && let Some(v) = property(conn, win, name, utf8)
        && !v.is_empty()
    {
        return String::from_utf8_lossy(&v).trim().to_string();
    }
    // WM_NAME is usually Latin-1 STRING (or COMPOUND_TEXT, close enough for ASCII titles).
    property(conn, win, AtomEnum::WM_NAME.into(), AtomEnum::ANY)
        .map(|v| v.iter().map(|&b| b as char).collect::<String>().trim().to_string())
        .unwrap_or_default()
}

/// XComposite 0.2+ (NameWindowPixmap) is available.
fn has_composite(conn: &RustConnection) -> bool {
    conn.extension_information(x11rb::protocol::composite::X11_EXTENSION_NAME).ok().flatten().is_some()
        && conn
            .composite_query_version(0, 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| v.major_version > 0 || v.minor_version >= 2)
}

/// Visible application windows, topmost last as the window manager lists them.
pub(super) fn list_windows(conn: &RustConnection, root: Window) -> Vec<ScreenInfo> {
    if !has_composite(conn) {
        return Vec::new();
    }
    let a = Atoms::get(conn);
    let mut wins = a.client_list.map(|cl| property32(conn, root, cl, AtomEnum::WINDOW)).unwrap_or_default();
    if wins.is_empty() {
        // No (EWMH) window manager: top-level windows are the root's children.
        wins = conn.query_tree(root).ok().and_then(|c| c.reply().ok()).map(|t| t.children).unwrap_or_default();
    }
    let own_pid = std::process::id();
    let mut out = Vec::new();
    for win in wins {
        let Some(attrs) = conn.get_window_attributes(win).ok().and_then(|c| c.reply().ok()) else { continue };
        // Minimised windows are unmapped and have no picture to capture.
        if attrs.map_state != MapState::VIEWABLE || attrs.override_redirect {
            continue;
        }
        if a.pid.is_some_and(|p| property32(conn, win, p, AtomEnum::CARDINAL).first() == Some(&own_pid)) {
            continue;
        }
        if let Some(t) = a.wm_type
            && property32(conn, win, t, AtomEnum::ATOM).iter().any(|ty| a.skip_types.contains(ty))
        {
            continue;
        }
        let Some(g) = conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) else { continue };
        if g.width < 32 || g.height < 32 {
            continue;
        }
        let name = title(conn, &a, win);
        if name.is_empty() {
            continue;
        }
        out.push(ScreenInfo {
            id: format!("window:{win}"),
            name,
            kind: ScreenKind::Window,
            width: g.width as u32,
            height: g.height as u32,
            primary: false,
        });
    }
    out
}

/// The window's current pixmap and its size.
struct Named {
    pixmap: u32,
    w: u16,
    h: u16,
}

const CLOSED: &str = "the window was closed";

pub(super) fn run_window(
    id: &str,
    config: ScreenConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let setup = (|| -> Result<_, String> {
        let win: Window = id.parse().map_err(|_| "unknown window")?;
        let (conn, root) = connect()?;
        if !has_composite(&conn) {
            return Err("window capture needs the X Composite extension".into());
        }
        let g = conn.get_geometry(win).map_err(|e| e.to_string())?.reply().map_err(|_| CLOSED.to_string())?;
        let s = conn.setup();
        let bpp = s.pixmap_formats.iter().find(|f| f.depth == g.depth).map(|f| f.bits_per_pixel);
        if !(g.depth == 24 || g.depth == 32) || bpp != Some(32) || s.image_byte_order != ImageOrder::LSB_FIRST {
            return Err(format!("unsupported window pixel format (depth {}, {bpp:?} bpp)", g.depth));
        }
        // Resize, unmap and destroy notifications for this window.
        conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|_| CLOSED.to_string())?;
        conn.composite_redirect_window(win, Redirect::AUTOMATIC)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| format!("cannot capture that window: {e:?}"))?;
        Ok((conn, root, win))
    })();
    let (conn, root, win) = match setup {
        Ok(v) => v,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    let has_shm = conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
    let xfixes = config.show_cursor && has_xfixes(&conn);
    ready(Ok(()));

    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    let mut pointer = Pointer::default();
    let mut shm: Option<ShmSeg> = None;
    let mut named: Option<Named> = None;
    let mut mapped = true;
    let mut result = Ok(());
    'run: while !stop.stopped() {
        let wait = pacer.wait(now_us());
        if !wait.is_zero() && stop.sleep(wait) {
            break;
        }
        while let Ok(Some(ev)) = conn.poll_for_event() {
            match ev {
                Event::DestroyNotify(e) if e.window == win => {
                    result = Err(CLOSED.into());
                    break 'run;
                }
                Event::UnmapNotify(e) if e.window == win => mapped = false,
                Event::MapNotify(e) if e.window == win => {
                    mapped = true;
                    free(&conn, named.take());
                }
                // A resized window gets a new pixmap.
                Event::ConfigureNotify(e)
                    if e.window == win && named.as_ref().is_some_and(|n| (n.w, n.h) != (e.width, e.height)) =>
                {
                    free(&conn, named.take());
                }
                _ => {}
            }
        }
        let t = now_us();
        if !mapped || !pacer.accept(t) {
            // Minimised or hidden: nothing to show (the sender repeats the last picture).
            continue;
        }
        if named.is_none() {
            named = name_pixmap(&conn, win);
            if named.is_none() {
                if conn.get_geometry(win).ok().and_then(|c| c.reply().ok()).is_none() {
                    result = Err(CLOSED.into());
                    break;
                }
                continue;
            }
        }
        let Some(n) = &named else { continue };
        let (w, h) = (n.w as usize, n.h as usize);
        let size = w * h * 4;
        if has_shm && shm.as_ref().is_none_or(|s| s.size < size) {
            if let Some(s) = shm.take() {
                s.detach(&conn);
            }
            shm = ShmSeg::new(&conn, size);
        }
        let owned;
        let data: &[u8] = match &shm {
            Some(s) => {
                let r = conn
                    .shm_get_image(n.pixmap, 0, 0, n.w, n.h, !0, ImageFormat::Z_PIXMAP.into(), s.seg, 0)
                    .map_err(drop)
                    .and_then(|c| c.reply().map_err(drop));
                if r.is_err() {
                    // Usually a resize or unmap racing us; the events tell on the next round.
                    free(&conn, named.take());
                    continue;
                }
                unsafe { std::slice::from_raw_parts(s.addr, size) }
            }
            None => {
                match conn
                    .get_image(ImageFormat::Z_PIXMAP, n.pixmap, 0, 0, n.w, n.h, !0)
                    .map_err(drop)
                    .and_then(|c| c.reply().map_err(drop))
                {
                    Ok(r) if r.data.len() >= size => owned = r.data,
                    _ => {
                        free(&conn, named.take());
                        continue;
                    }
                }
                &owned
            }
        };
        conv.convert(
            &RawFrame::Bgra { data, stride: w * 4, width: w, height: h },
            config.max_width,
            config.max_height,
            &mut frame,
        );
        if xfixes && let Some(o) = conn.translate_coordinates(win, root, 0, 0).ok().and_then(|c| c.reply().ok()) {
            pointer.draw(&conn, &mut frame, o.dst_x as i32, o.dst_y as i32, w, h);
        }
        frame.timestamp_us = t;
        sink(&frame);
    }
    free(&conn, named.take());
    if let Some(s) = shm.take() {
        s.detach(&conn);
    }
    let _ = conn.composite_unredirect_window(win, Redirect::AUTOMATIC);
    let _ = conn.flush();
    result
}

fn name_pixmap(conn: &RustConnection, win: Window) -> Option<Named> {
    let g = conn.get_geometry(win).ok()?.reply().ok()?;
    let pixmap = conn.generate_id().ok()?;
    conn.composite_name_window_pixmap(win, pixmap).ok()?.check().ok()?;
    Some(Named { pixmap, w: g.width.max(1), h: g.height.max(1) })
}

fn free(conn: &RustConnection, n: Option<Named>) {
    if let Some(n) = n {
        let _ = conn.free_pixmap(n.pixmap);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, PropMode, WindowClass};
    use x11rb::wrapper::ConnectionExt as _;

    use crate::capture::{Capture, ScreenConfig, ScreenKind, list_screens};

    fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Lists, captures, resizes, hides and closes a window of our own. Runs against a real X
    /// server when `DISPLAY` is set (e.g. `Xvfb :99 &` and `DISPLAY=:99 cargo test`).
    #[test]
    fn captures_an_x11_window() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        // Xvfb resets when its last client leaves (unless -noreset), dropping connections made
        // at that moment; the display test runs in parallel, so retry.
        let (conn, screen) = (0..20)
            .find_map(|_| {
                x11rb::connect(None).ok().or_else(|| {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                })
            })
            .expect("connect to the X server");
        let s = &conn.setup().roots[screen];
        let win = conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().background_pixel(0xff0000); // red
        conn.create_window(0, win, s.root, 10, 10, 200, 120, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        let title = format!("celerity window test {}", std::process::id());
        conn.change_property8(
            PropMode::REPLACE,
            win,
            super::AtomEnum::WM_NAME,
            super::AtomEnum::STRING,
            title.as_bytes(),
        )
        .unwrap();
        // A second window claiming to be ours (by pid) must not be listed.
        let own = conn.generate_id().unwrap();
        conn.create_window(0, own, s.root, 0, 0, 100, 100, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        conn.change_property8(PropMode::REPLACE, own, super::AtomEnum::WM_NAME, super::AtomEnum::STRING, b"ours")
            .unwrap();
        let pid_atom = conn.intern_atom(false, b"_NET_WM_PID").unwrap().reply().unwrap().atom;
        conn.change_property32(PropMode::REPLACE, own, pid_atom, super::AtomEnum::CARDINAL, &[std::process::id()])
            .unwrap();
        conn.map_window(win).unwrap();
        conn.map_window(own).unwrap();
        conn.flush().unwrap();

        let mut found = None;
        wait_for("the window to be listed", 5, || {
            let list = list_screens().unwrap();
            assert!(!list.iter().any(|w| w.name == "ours"), "own window listed");
            found = list.into_iter().find(|w| w.name == title);
            found.is_some()
        });
        let info = found.unwrap();
        assert_eq!((info.kind, info.width, info.height), (ScreenKind::Window, 200, 120));

        type Seen = (usize, u32, u32, u8, u8); // frames, size, centre Y and V
        let got: Arc<Mutex<Seen>> = Arc::default();
        let g2 = got.clone();
        let cfg = ScreenConfig { max_width: 640, max_height: 360, fps: 30, show_cursor: false };
        let cap = Capture::screen(&info.id, cfg, move |f| {
            let mut g = g2.lock().unwrap();
            *g = (g.0 + 1, f.width(), f.height(), f.y()[f.y().len() / 2], f.v()[f.v().len() / 2]);
        })
        .expect("start window capture");
        wait_for("frames", 5, || got.lock().unwrap().0 >= 3);
        let (_, w, h, y, v) = *got.lock().unwrap();
        assert_eq!((w, h), (200, 120));
        // Red in BT.709 limited range: Y 63, V 240.
        assert!((y as i32 - 63).abs() <= 3 && v >= 230, "not red: Y {y} V {v}");

        conn.configure_window(win, &ConfigureWindowAux::new().width(300).height(100)).unwrap();
        conn.flush().unwrap();
        wait_for("the resized picture", 5, || {
            let g = got.lock().unwrap();
            (g.1, g.2) == (300, 100)
        });

        conn.unmap_window(win).unwrap();
        conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let before = got.lock().unwrap().0;
        std::thread::sleep(Duration::from_millis(300));
        assert!(got.lock().unwrap().0 <= before + 1, "frames while unmapped");
        conn.map_window(win).unwrap();
        conn.flush().unwrap();
        wait_for("frames after mapping again", 5, || got.lock().unwrap().0 > before + 2);

        conn.destroy_window(win).unwrap();
        conn.destroy_window(own).unwrap();
        conn.flush().unwrap();
        wait_for("the capture to stop", 5, || !cap.is_running());
        assert_eq!(cap.error().as_deref(), Some(super::CLOSED));
        assert!(Capture::screen(&info.id, cfg, |_| {}).is_err());
    }
}
