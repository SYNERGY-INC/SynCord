//! X11 screen capture: RandR monitors, MIT-SHM `GetImage` (plain `GetImage` when the server is
//! remote or lacks SHM), XFixes for the pointer.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

use crate::capture::{Pacer, Ready, ScreenConfig, ScreenInfo, ScreenKind, Sink, Stop};
use crate::convert::{Converter, CursorImage, RawFrame, overlay_cursor};
use crate::frame::I420Frame;
use crate::now_us;

#[derive(Clone, Debug)]
struct Monitor {
    name: String,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    primary: bool,
}

impl Monitor {
    fn id(&self) -> String {
        format!("{}|{}|{}|{}|{}", self.name, self.x, self.y, self.w, self.h)
    }
}

fn connect() -> Result<(RustConnection, Window), String> {
    if std::env::var_os("DISPLAY").is_none() {
        return Err(if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "screen sharing needs an X11 session (Wayland capture is not supported yet)".into()
        } else {
            "no X11 display (DISPLAY is not set)".into()
        });
    }
    let (conn, screen) = x11rb::connect(None).map_err(|e| format!("cannot connect to the X server: {e}"))?;
    let root = conn.setup().roots[screen].root;
    if conn.extension_information("XWAYLAND").ok().flatten().is_some()
        && std::env::var_os("SYNCORD_ALLOW_XWAYLAND_CAPTURE").is_none()
    {
        // XWayland only sees X11 windows; the rest of the desktop would be black.
        return Err("screen sharing needs an X11 session (on Wayland only XWayland windows would be visible)".into());
    }
    Ok((conn, root))
}

fn monitors(conn: &RustConnection, root: Window) -> Result<Vec<Monitor>, String> {
    let mut out = Vec::new();
    let has_randr = conn
        .randr_query_version(1, 5)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| v.major_version > 1 || v.minor_version >= 5);
    if has_randr
        && let Ok(reply) = conn
            .randr_get_monitors(root, true)
            .map_err(|e| e.to_string())
            .and_then(|c| c.reply().map_err(|e| e.to_string()))
    {
        for m in reply.monitors {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| "Display".into());
            if m.width > 0 && m.height > 0 {
                out.push(Monitor { name, x: m.x, y: m.y, w: m.width, h: m.height, primary: m.primary });
            }
        }
    }
    if out.is_empty() {
        let g = conn.get_geometry(root).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        out.push(Monitor { name: "Screen".into(), x: 0, y: 0, w: g.width, h: g.height, primary: true });
    }
    if !out.iter().any(|m| m.primary) {
        out[0].primary = true;
    }
    Ok(out)
}

pub(crate) fn list_screens() -> Result<Vec<ScreenInfo>, String> {
    let (conn, root) = connect()?;
    let mons = monitors(&conn, root)?;
    let many = mons.len() > 1;
    Ok(mons
        .iter()
        .enumerate()
        .map(|(i, m)| ScreenInfo {
            id: m.id(),
            name: if many { format!("Display {} ({})", i + 1, m.name) } else { format!("Display ({})", m.name) },
            kind: ScreenKind::Display,
            width: m.w as u32,
            height: m.h as u32,
            primary: m.primary,
        })
        .collect())
}

/// A System V shared memory segment attached to the X server.
struct ShmSeg {
    seg: u32,
    addr: *mut u8,
    size: usize,
}

impl ShmSeg {
    fn new(conn: &RustConnection, size: usize) -> Option<ShmSeg> {
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return None;
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            if addr as isize == -1 {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return None;
            }
            let seg = conn.generate_id().ok();
            let attached = seg.and_then(|seg| conn.shm_attach(seg, id as u32, false).ok()?.check().ok());
            // Marked for removal now; it lives until both we and the server detach.
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            match (seg, attached) {
                (Some(seg), Some(())) => Some(ShmSeg { seg, addr: addr.cast(), size }),
                _ => {
                    libc::shmdt(addr);
                    None
                }
            }
        }
    }

    fn detach(self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg).map(|c| c.check());
        unsafe { libc::shmdt(self.addr.cast()) };
    }
}

pub(crate) fn run_screen(
    id: &str,
    config: ScreenConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let setup = (|| -> Result<_, String> {
        let (conn, root) = connect()?;
        let mons = monitors(&conn, root)?;
        let name = id.split('|').next().unwrap_or("");
        let mon = mons
            .iter()
            .find(|m| m.id() == id)
            .or_else(|| mons.iter().find(|m| m.name == name))
            .or_else(|| if id.is_empty() { mons.iter().find(|m| m.primary) } else { None })
            .cloned()
            .ok_or("that display is no longer connected")?;
        let s = conn.setup();
        let root_screen = s.roots.iter().find(|r| r.root == root).ok_or("no X screen")?;
        let depth = root_screen.root_depth;
        let bpp = s.pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel);
        if !(depth == 24 || depth == 32) || bpp != Some(32) || s.image_byte_order != ImageOrder::LSB_FIRST {
            return Err(format!("unsupported X11 pixel format (depth {depth}, {bpp:?} bpp)"));
        }
        Ok((conn, root, mon))
    })();
    let (conn, root, mon) = match setup {
        Ok(v) => v,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    let size = mon.w as usize * mon.h as usize * 4;
    let has_shm = conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
    let mut shm = if has_shm { ShmSeg::new(&conn, size) } else { None };
    let has_xfixes = config.show_cursor && conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
    ready(Ok(()));

    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    let mut cursor = CursorImage::default();
    let mut cursor_serial = None;
    let (w, h) = (mon.w as usize, mon.h as usize);
    let mut result = Ok(());
    while !stop.stopped() {
        let wait = pacer.wait(now_us());
        if !wait.is_zero() && stop.sleep(wait) {
            break;
        }
        let t = now_us();
        if !pacer.accept(t) {
            continue;
        }
        let owned;
        let data: &[u8] = match &shm {
            Some(s) => {
                let r = conn
                    .shm_get_image(root, mon.x, mon.y, mon.w, mon.h, !0, ImageFormat::Z_PIXMAP.into(), s.seg, 0)
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.reply().map_err(|e| e.to_string()));
                if let Err(e) = r {
                    // Display reconfigured (smaller root) or connection lost.
                    result = Err(format!("screen capture stopped: {e}"));
                    break;
                }
                unsafe { std::slice::from_raw_parts(s.addr, s.size) }
            }
            None => {
                match conn
                    .get_image(ImageFormat::Z_PIXMAP, root, mon.x, mon.y, mon.w, mon.h, !0)
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.reply().map_err(|e| e.to_string()))
                {
                    Ok(r) => owned = r.data,
                    Err(e) => {
                        result = Err(format!("screen capture stopped: {e}"));
                        break;
                    }
                }
                &owned
            }
        };
        if data.len() < size {
            result = Err("screen capture stopped: short image".into());
            break;
        }
        conv.convert(
            &RawFrame::Bgra { data, stride: w * 4, width: w, height: h },
            config.max_width,
            config.max_height,
            &mut frame,
        );
        if has_xfixes && let Ok(c) = conn.xfixes_get_cursor_image().map_err(drop).and_then(|c| c.reply().map_err(drop))
        {
            if cursor_serial != Some(c.cursor_serial) {
                cursor_serial = Some(c.cursor_serial);
                cursor = CursorImage {
                    width: c.width as usize,
                    height: c.height as usize,
                    hot_x: c.xhot as i32,
                    hot_y: c.yhot as i32,
                    rgba: c.cursor_image.iter().flat_map(|&p| unpremultiply(p)).collect(),
                    invert: Vec::new(),
                };
            }
            let (cx, cy) = (c.x as i32 - mon.x as i32, c.y as i32 - mon.y as i32);
            overlay_cursor(&mut frame, &cursor, cx, cy, w, h);
        }
        frame.timestamp_us = t;
        sink(&frame);
    }
    if let Some(s) = shm.take() {
        s.detach(&conn);
    }
    result
}

/// XFixes cursor pixels are premultiplied ARGB32.
fn unpremultiply(p: u32) -> [u8; 4] {
    let a = (p >> 24) as u8;
    let un = |c: u32| if a == 0 { 0 } else { ((c & 0xff) * 255 / a as u32).min(255) as u8 };
    [un(p >> 16), un(p >> 8), un(p), a]
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::capture::{Capture, ScreenConfig, list_screens};
    use crate::frame::I420Frame;

    #[test]
    fn cursor_unpremultiply() {
        assert_eq!(unpremultiply_test(0x80_40_20_10), [0x7f, 0x3f, 0x1f, 0x80]);
        assert_eq!(unpremultiply_test(0), [0, 0, 0, 0]);
    }

    fn unpremultiply_test(p: u32) -> [u8; 4] {
        super::unpremultiply(p)
    }

    /// Runs against a real X server when `DISPLAY` is set (e.g. `Xvfb :99 &` and
    /// `DISPLAY=:99 cargo test`); skipped otherwise.
    #[test]
    fn captures_x11_display() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        let screens = list_screens().expect("list screens");
        assert!(!screens.is_empty());
        let s = screens.iter().find(|s| s.primary).unwrap();
        let got: Arc<Mutex<(usize, I420Frame)>> = Arc::default();
        let g2 = got.clone();
        let cfg = ScreenConfig { max_width: 640, max_height: 360, fps: 20, show_cursor: true };
        let cap = Capture::screen(&s.id, cfg, move |f| {
            let mut g = g2.lock().unwrap();
            g.0 += 1;
            g.1.copy_from(f);
        })
        .expect("start capture");
        std::thread::sleep(Duration::from_millis(1000));
        assert!(cap.is_running() && cap.error().is_none());
        drop(cap);
        let g = got.lock().unwrap();
        // Debug builds convert slowly, so only check that frames flow and are paced.
        assert!((1..=22).contains(&g.0), "{} frames in 1 s at 20 fps", g.0);
        let (w, h) = crate::convert::fit_size(s.width, s.height, 640, 360);
        assert_eq!((g.1.width(), g.1.height()), (w, h));
        // Wrong display ids fail cleanly.
        assert!(Capture::screen("nope|1|2|3|4", cfg, |_| {}).is_err());
    }
}
