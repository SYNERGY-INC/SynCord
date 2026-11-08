//! Display capture with DXGI desktop duplication (Windows 8+): the desktop image is copied on
//! the GPU into a staging texture, read once per captured frame and converted straight to I420.
//! Only changed frames are taken (the call waits for the next desktop update). The pointer is
//! drawn by us from the shape DXGI reports. When duplication is unavailable (some remote / VM
//! setups, too many duplicators), GDI `BitBlt` is used instead.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{HMODULE, POINT, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::Interface;

use crate::capture::{Pacer, Ready, ScreenConfig, ScreenInfo, ScreenKind, Sink, Stop};
use crate::convert::{Converter, CursorImage, RawFrame, overlay_cursor};
use crate::frame::I420Frame;
use crate::now_us;

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

struct Output {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    /// GDI device name, e.g. `\\.\DISPLAY1`: our display id.
    device: String,
    rect: RECT,
}

fn outputs() -> Result<Vec<Output>, String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("DXGI unavailable: {}", e.message()))?;
        let mut out = Vec::new();
        for a in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let Ok(desc) = output.GetDesc() else { continue };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                // The same monitor can show up under several adapters (hybrid GPUs); keep the first.
                let device = wide(&desc.DeviceName);
                if out.iter().any(|x: &Output| x.device == device) {
                    continue;
                }
                out.push(Output { adapter: adapter.clone(), output, device, rect: desc.DesktopCoordinates });
            }
        }
        Ok(out)
    }
}

/// Monitor model names ("DELL U2719D") by GDI device name, from the display configuration.
fn friendly_names() -> Vec<(String, String)> {
    let mut names = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            .is_err()
        {
            return names;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) == 0 && DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 {
                let friendly = wide(&tgt.monitorFriendlyDeviceName);
                if !friendly.is_empty() {
                    names.push((wide(&src.viewGdiDeviceName), friendly));
                }
            }
        }
    }
    names
}

pub(crate) fn list_screens() -> Result<Vec<ScreenInfo>, String> {
    let outs = outputs()?;
    let names = friendly_names();
    let mut list: Vec<ScreenInfo> = outs
        .iter()
        .map(|o| {
            let r = o.rect;
            let num = o.device.trim_start_matches(r"\\.\DISPLAY").to_string();
            let model = names.iter().find(|(d, _)| *d == o.device).map(|(_, n)| n.clone());
            let name = match model {
                Some(m) => format!("Display {num} ({m})"),
                None => format!("Display {num}"),
            };
            ScreenInfo {
                id: o.device.clone(),
                name,
                kind: ScreenKind::Display,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                primary: r.left == 0 && r.top == 0,
            }
        })
        .collect();
    list.sort_by_key(|s| !s.primary);
    Ok(list)
}

/// Desktop duplication state for one output.
struct Dupl {
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    /// The staging texture holds a desktop image.
    have_image: bool,
}

unsafe fn open_dupl(o: &Output) -> windows::core::Result<Dupl> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_9_3];
        // The device must live on the adapter that owns the output (hybrid GPU laptops).
        D3D11CreateDevice(
            &o.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let ctx = ctx.ok_or_else(windows::core::Error::empty)?;
        let output1: IDXGIOutput1 = o.output.cast()?;
        let dup = output1.DuplicateOutput(&device)?;
        let desc = dup.GetDesc();
        let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        let td = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&td, None, Some(&mut staging))?;
        let staging = staging.ok_or_else(windows::core::Error::empty)?;
        Ok(Dupl { ctx, dup, staging, width, height, have_image: false })
    }
}

/// Pointer state from duplication frames.
#[derive(Default)]
struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    shape: CursorImage,
    buf: Vec<u8>,
}

impl Pointer {
    unsafe fn update(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.visible = info.PointerPosition.Visible.as_bool();
            self.x = info.PointerPosition.Position.x;
            self.y = info.PointerPosition.Position.y;
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        self.buf.resize(info.PointerShapeBufferSize as usize, 0);
        let mut needed = 0u32;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let ok = unsafe {
            dup.GetFramePointerShape(self.buf.len() as u32, self.buf.as_mut_ptr() as *mut c_void, &mut needed, &mut si)
        };
        if ok.is_ok() {
            self.shape = pointer_image(&self.buf, &si);
        }
    }
}

/// Converts a DXGI pointer shape to straight-alpha RGBA plus an invert mask.
fn pointer_image(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> CursorImage {
    let (w, pitch) = (si.Width as usize, si.Pitch as usize);
    let mut img = CursorImage { width: w, hot_x: si.HotSpot.x, hot_y: si.HotSpot.y, ..Default::default() };
    match si.Type {
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => {
            let h = si.Height as usize;
            img.height = h;
            for y in 0..h {
                for x in 0..w {
                    let p = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4).unwrap_or(&[0; 4]);
                    img.rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
        }
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
            let h = si.Height as usize;
            img.height = h;
            for y in 0..h {
                for x in 0..w {
                    let p = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4).unwrap_or(&[0; 4]);
                    // Mask byte 0: opaque colour. 0xFF: XOR with the screen (black = no change).
                    let xor = p[3] != 0;
                    let blank = xor && p[0] == 0 && p[1] == 0 && p[2] == 0;
                    img.rgba.extend_from_slice(&[p[2], p[1], p[0], if xor { 0 } else { 255 }]);
                    img.invert.push(xor && !blank);
                }
            }
        }
        _ => {
            // Monochrome: AND mask on top, XOR mask below, 1 bit per pixel.
            let h = si.Height as usize / 2;
            img.height = h;
            let bit = |row: usize, x: usize| buf.get(row * pitch + x / 8).is_some_and(|b| b & (0x80 >> (x % 8)) != 0);
            for y in 0..h {
                for x in 0..w {
                    let (and, xor) = (bit(y, x), bit(y + h, x));
                    let (rgba, inv) = match (and, xor) {
                        (false, false) => ([0, 0, 0, 255], false),
                        (false, true) => ([255, 255, 255, 255], false),
                        (true, false) => ([0, 0, 0, 0], false),
                        (true, true) => ([0, 0, 0, 0], true),
                    };
                    img.rgba.extend_from_slice(&rgba);
                    img.invert.push(inv);
                }
            }
        }
    }
    img
}

pub(crate) fn run_screen(
    id: &str,
    config: ScreenConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let found = outputs().and_then(|outs| {
        outs.into_iter()
            .find(|o| o.device == id)
            .or_else(|| {
                if id.is_empty() {
                    outputs().ok()?.into_iter().find(|o| o.rect.left == 0 && o.rect.top == 0)
                } else {
                    None
                }
            })
            .ok_or_else(|| "that display is no longer connected".to_string())
    });
    let out = match found {
        Ok(o) => o,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    match unsafe { open_dupl(&out) } {
        Ok(d) => {
            ready(Ok(()));
            unsafe { dupl_loop(&out, d, config, &mut sink, stop) }
        }
        Err(_) => {
            // Duplication unavailable: GDI works everywhere, at a higher CPU cost.
            unsafe { gdi_loop(&out, config, &mut sink, ready, stop) }
        }
    }
}

unsafe fn dupl_loop(out: &Output, d: Dupl, config: ScreenConfig, sink: &mut Sink, stop: &Stop) -> Result<(), String> {
    let mut dupl = Some(d);
    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    let mut ptr = Pointer::default();
    let mut lost_since: Option<Instant> = None;
    while !stop.stopped() {
        let wait = pacer.wait(now_us());
        if !wait.is_zero() && stop.sleep(wait) {
            break;
        }
        let Some(d) = dupl.as_mut() else {
            // Lost (mode change, secure desktop such as UAC or the lock screen, fullscreen
            // switch): the old duplication is released; keep trying to get a new one.
            let since = *lost_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(60) {
                return Err("screen capture lost access to the display".into());
            }
            if stop.sleep(Duration::from_millis(250)) {
                break;
            }
            if let Ok(nd) = unsafe { open_dupl(out) } {
                dupl = Some(nd);
                lost_since = None;
            }
            continue;
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let r = unsafe { d.dup.AcquireNextFrame(100, &mut info, &mut res) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue, // nothing changed
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                drop(res);
                dupl = None;
                continue;
            }
            Err(e) => return Err(format!("screen capture failed: {}", e.message())),
        }
        let t = now_us();
        unsafe {
            if info.LastPresentTime != 0
                && let Some(tex) = res.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok())
            {
                d.ctx.CopyResource(&d.staging, &tex);
                d.have_image = true;
            }
            if config.show_cursor {
                ptr.update(&d.dup, &info);
            }
            drop(res);
            let _ = d.dup.ReleaseFrame();
        }
        // Pointer-only updates are worth a frame only when the pointer is drawn.
        if !d.have_image || (info.LastPresentTime == 0 && !config.show_cursor) || !pacer.accept(t) {
            continue;
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { d.ctx.Map(&d.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_err() {
            continue;
        }
        let (w, h) = (d.width as usize, d.height as usize);
        let pitch = mapped.RowPitch as usize;
        let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (h - 1) + w * 4) };
        conv.convert(
            &RawFrame::Bgra { data, stride: pitch, width: w, height: h },
            config.max_width,
            config.max_height,
            &mut frame,
        );
        unsafe { d.ctx.Unmap(&d.staging, 0) };
        if config.show_cursor && ptr.visible {
            overlay_cursor(&mut frame, &ptr.shape, ptr.x + ptr.shape.hot_x, ptr.y + ptr.shape.hot_y, w, h);
        }
        frame.timestamp_us = t;
        sink(&frame);
    }
    Ok(())
}

unsafe fn gdi_loop(
    out: &Output,
    config: ScreenConfig,
    sink: &mut Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let r = out.rect;
    let (w, h) = ((r.right - r.left).max(0), (r.bottom - r.top).max(0));
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() || w < 2 || h < 2 {
            ready(Err("screen capture is not available on this system".into()));
            return Ok(());
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                ready(Err("screen capture is not available on this system".into()));
                return Ok(());
            }
        };
        let old = SelectObject(mem, dib.into());
        ready(Ok(()));
        let mut conv = Converter::default();
        let mut frame = I420Frame::default();
        let mut pacer = Pacer::new(config.fps);
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
            if BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT).is_err() {
                // Secure desktop or display change; try again shortly.
                continue;
            }
            if config.show_cursor {
                let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
                if GetCursorInfo(&mut ci).is_ok() && ci.flags.0 & CURSOR_SHOWING.0 != 0 {
                    let mut ii = ICONINFO::default();
                    let (mut hx, mut hy) = (0, 0);
                    if GetIconInfo(ci.hCursor.into(), &mut ii).is_ok() {
                        hx = ii.xHotspot as i32;
                        hy = ii.yHotspot as i32;
                        if !ii.hbmMask.is_invalid() {
                            let _ = DeleteObject(ii.hbmMask.into());
                        }
                        if !ii.hbmColor.is_invalid() {
                            let _ = DeleteObject(ii.hbmColor.into());
                        }
                    }
                    let POINT { x, y } = ci.ptScreenPos;
                    let _ =
                        DrawIconEx(mem, x - r.left - hx, y - r.top - hy, ci.hCursor.into(), 0, 0, 0, None, DI_NORMAL);
                }
            }
            let _ = GdiFlush();
            let data = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let (uw, uh) = (w as usize, h as usize);
            conv.convert(
                &RawFrame::Bgra { data, stride: uw * 4, width: uw, height: uh },
                config.max_width,
                config.max_height,
                &mut frame,
            );
            frame.timestamp_us = t;
            sink(&frame);
            if frame.width() == 0 {
                result = Err("screen capture failed".into());
                break;
            }
        }
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}
