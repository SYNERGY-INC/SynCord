//! Window capture with Windows.Graphics.Capture (Windows 10 1903+): the compositor hands us the
//! window's own surface (also when other windows cover it) as a GPU texture; we copy it to a
//! staging texture and convert BGRA -> I420 like DXGI display capture does.

use std::time::Duration;

use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Interface};

use crate::capture::{Pacer, Ready, ScreenConfig, ScreenInfo, ScreenKind, Sink, Stop};
use crate::convert::{Converter, RawFrame};
use crate::frame::I420Frame;
use crate::now_us;

const CLOSED: &str = "the window was closed";

fn hwnd_of(id: &str) -> Option<HWND> {
    id.parse::<isize>().ok().map(|v| HWND(v as *mut _))
}

/// The process that owns a shared window (for per-application audio).
pub(crate) fn window_pid(id: &str) -> Option<u32> {
    let hwnd = hwnd_of(id.strip_prefix("window:")?)?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn title(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize]).trim().to_string()
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Hidden by the shell (other virtual desktops, suspended UWP apps): visible to `IsWindowVisible`
/// but not on screen.
fn cloaked(hwnd: HWND) -> bool {
    let mut v = 0u32;
    let r =
        unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut v).cast(), std::mem::size_of::<u32>() as u32) };
    r.is_ok() && v != 0
}

/// Size in pixels as shown (DWM frame bounds); the restored size for minimised windows.
fn window_size(hwnd: HWND) -> (u32, u32) {
    unsafe {
        let mut r = RECT::default();
        if IsIconic(hwnd).as_bool() {
            let mut wp =
                WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                r = wp.rcNormalPosition;
            }
        } else if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut r);
        }
        ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
    }
}

/// Top-level application windows in z-order (topmost first), as the taskbar would show them.
pub(crate) fn list_windows() -> Vec<ScreenInfo> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Vec` passed to EnumWindows below, alive for the call.
        unsafe { (*(lparam.0 as *mut Vec<HWND>)).push(hwnd) };
        BOOL(1)
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&raw mut all as isize));
    }
    let own_pid = std::process::id();
    let mut out = Vec::new();
    for hwnd in all {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || cloaked(hwnd) {
                continue;
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            // Owned windows (dialogs, palettes) belong to their owner unless they ask to be shown.
            let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.0.is_null());
            if owned && ex & WS_EX_APPWINDOW.0 == 0 {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == own_pid {
                continue;
            }
        }
        if matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            continue;
        }
        let name = title(hwnd);
        let (w, h) = window_size(hwnd);
        if name.is_empty() || w < 2 || h < 2 {
            continue;
        }
        out.push(ScreenInfo {
            id: format!("window:{}", hwnd.0 as isize),
            name,
            kind: ScreenKind::Window,
            width: w,
            height: h,
            primary: false,
        });
    }
    out
}

/// Windows.Graphics.Capture objects, closed on drop (closing stops the capture at once rather
/// than when the garbage collector-like reference counting gets to it).
struct Wgc {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// `RoInitialize` for this thread, undone on drop.
struct WinRt(bool);

impl Drop for WinRt {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() };
        }
    }
}

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    d3d: IDirect3DDevice,
}

unsafe fn gpu() -> windows::core::Result<Gpu> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let ctx = ctx.ok_or_else(windows::core::Error::empty)?;
        let dxgi: IDXGIDevice = device.cast()?;
        let d3d: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        Ok(Gpu { device, ctx, d3d })
    }
}

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

fn start(gpu: &Gpu, hwnd: HWND, show_cursor: bool) -> windows::core::Result<(Wgc, SizeInt32)> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.d3d, FORMAT, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    // Both are newer than WGC itself (cursor: Windows 10 2004, border: Windows 11); older
    // systems keep their defaults (cursor shown, yellow border).
    let _ = session.SetIsCursorCaptureEnabled(show_cursor);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;
    Ok((Wgc { pool, session }, size))
}

pub(crate) fn run_window(
    id: &str,
    config: ScreenConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let Some(hwnd) = hwnd_of(id) else {
        ready(Err("unknown window".into()));
        return Ok(());
    };
    let _rt = WinRt(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok());
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        ready(Err(CLOSED.into()));
        return Ok(());
    }
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        ready(Err("window capture needs Windows 10 version 1903 or later".into()));
        return Ok(());
    }
    let gpu = match unsafe { gpu() } {
        Ok(g) => g,
        Err(e) => {
            ready(Err(format!("window capture needs a Direct3D 11 GPU: {}", e.message())));
            return Ok(());
        }
    };
    let (wgc, mut pool_size) = match start(&gpu, hwnd, config.show_cursor) {
        Ok(v) => v,
        Err(e) => {
            ready(Err(format!("cannot capture that window: {}", e.message())));
            return Ok(());
        }
    };
    ready(Ok(()));

    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    let mut staging: Option<(ID3D11Texture2D, u32, u32)> = None;
    while !stop.stopped() {
        let wait = pacer.wait(now_us());
        let wait = if wait.is_zero() { Duration::from_millis(4) } else { wait };
        if stop.sleep(wait) {
            break;
        }
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(CLOSED.into());
        }
        // Newest frame only (the pool holds two; older ones are released as we go).
        let mut latest = None;
        while let Ok(f) = wgc.pool.TryGetNextFrame() {
            if let Some(old) = latest.replace(f) {
                let _ = old.Close();
            }
        }
        // None: unchanged, or minimised (WGC delivers nothing then; the sender repeats the
        // last picture).
        let Some(f) = latest else { continue };
        let t = now_us();
        if !pacer.accept(t) {
            let _ = f.Close();
            continue;
        }
        let content = f.ContentSize().unwrap_or(pool_size);
        if content.Width != pool_size.Width || content.Height != pool_size.Height {
            // Resized: future frames come at the new size; this one is still at the old one.
            if content.Width > 0 && content.Height > 0 && wgc.pool.Recreate(&gpu.d3d, FORMAT, 2, content).is_ok() {
                pool_size = content;
            }
        }
        let tex = f
            .Surface()
            .and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>())
            .and_then(|a| unsafe { a.GetInterface::<ID3D11Texture2D>() });
        let Ok(tex) = tex else {
            let _ = f.Close();
            continue;
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let w = (content.Width.max(0) as u32).min(desc.Width);
        let h = (content.Height.max(0) as u32).min(desc.Height);
        if w < 2 || h < 2 {
            let _ = f.Close();
            continue;
        }
        if staging.as_ref().is_none_or(|s| s.1 != w || s.2 != h) {
            staging = unsafe { staging_texture(&gpu.device, w, h) }.ok().map(|t| (t, w, h));
        }
        let Some((st, _, _)) = &staging else {
            let _ = f.Close();
            continue;
        };
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        unsafe { gpu.ctx.CopySubresourceRegion(st, 0, 0, 0, 0, &tex, 0, Some(&region)) };
        let _ = f.Close();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { gpu.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_err() {
            continue;
        }
        let (uw, uh) = (w as usize, h as usize);
        let pitch = mapped.RowPitch as usize;
        let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (uh - 1) + uw * 4) };
        conv.convert(
            &RawFrame::Bgra { data, stride: pitch, width: uw, height: uh },
            config.max_width,
            config.max_height,
            &mut frame,
        );
        unsafe { gpu.ctx.Unmap(st, 0) };
        frame.timestamp_us = t;
        sink(&frame);
    }
    drop(wgc);
    Ok(())
}

unsafe fn staging_texture(device: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<ID3D11Texture2D> {
    let td = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&td, None, Some(&mut t))? };
    t.ok_or_else(windows::core::Error::empty)
}
