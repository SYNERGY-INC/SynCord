//! ScreenCaptureKit (macOS 12.3+): the system scales to our size, converts to NV12 and only
//! delivers frames when something changed. Needs the Screen Recording permission.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFString, CGRect};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRectMakeWithDictionaryRepresentation, CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGWindowBounds,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use super::{AssertSend, FrameHandler};
use crate::capture::{Ready, ScreenConfig, ScreenInfo, ScreenKind, Sink, Stop};
use crate::convert::fit_size;

struct ScreenIvars {
    handler: FrameHandler,
    stopped: Arc<Mutex<Option<String>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = ScreenIvars]
    struct ScreenDelegate;

    unsafe impl NSObjectProtocol for ScreenDelegate {}

    unsafe impl SCStreamOutput for ScreenDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().handler.handle(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for ScreenDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!("screen capture stopped: {}", error.localizedDescription());
            *self.ivars().stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
        }
    }
);

impl ScreenDelegate {
    fn new(ivars: ScreenIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

const PERMISSION: &str = "screen recording permission needed: allow Celerity in System Settings > Privacy & Security > Screen Recording, then restart Celerity";

pub(crate) fn content() -> Result<Retained<SCShareableContent>, String> {
    if !CGPreflightScreenCaptureAccess() {
        // Shows the system prompt (once); the permission only applies after a restart.
        CGRequestScreenCaptureAccess();
        return Err(PERMISSION.into());
    }
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(AssertSend(c)),
            None => Err(unsafe { error.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "screen capture is unavailable".into())),
        };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("screen capture did not respond".into()),
    }
}

/// Size of a display in pixels (not points).
fn pixel_size(d: &SCDisplay) -> (u32, u32) {
    let id = unsafe { d.displayID() };
    let mode = CGDisplayCopyDisplayMode(id);
    match mode {
        Some(m) => (CGDisplayMode::pixel_width(Some(&m)) as u32, CGDisplayMode::pixel_height(Some(&m)) as u32),
        None => unsafe { (d.width().max(0) as u32, d.height().max(0) as u32) },
    }
}

pub(crate) fn list_screens() -> Result<Vec<ScreenInfo>, String> {
    let c = content()?;
    let main = CGMainDisplayID();
    let mut out = Vec::new();
    let displays: Vec<Retained<SCDisplay>> = unsafe { c.displays() }.to_vec();
    for (i, d) in displays.iter().enumerate() {
        let (w, h) = pixel_size(d);
        let id = unsafe { d.displayID() };
        out.push(ScreenInfo {
            id: format!("display:{id}"),
            name: if displays.len() > 1 { format!("Display {}", i + 1) } else { "Display".into() },
            kind: ScreenKind::Display,
            width: w,
            height: h,
            primary: id == main,
        });
    }
    out.sort_by_key(|s| !s.primary);
    let scale = displays.iter().find(|d| unsafe { d.displayID() } == main).map_or(2.0, |d| {
        let (pw, _) = pixel_size(d);
        pw as f64 / unsafe { d.width() }.max(1) as f64
    });
    let own_pid = std::process::id() as i32;
    for w in unsafe { c.windows() }.iter() {
        unsafe {
            if !w.isOnScreen() || w.windowLayer() != 0 {
                continue;
            }
            let Some(app) = w.owningApplication() else { continue };
            if app.processID() == own_pid {
                continue;
            }
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            let app_name = app.applicationName().to_string();
            let f = w.frame();
            if title.is_empty() || f.size.width < 64.0 || f.size.height < 64.0 {
                continue;
            }
            out.push(ScreenInfo {
                id: format!("window:{}", w.windowID()),
                name: if app_name.is_empty() { title } else { format!("{app_name}: {title}") },
                kind: ScreenKind::Window,
                width: (f.size.width * scale) as u32,
                height: (f.size.height * scale) as u32,
                primary: false,
            });
        }
    }
    Ok(out)
}

pub(crate) fn run_screen(id: &str, config: ScreenConfig, sink: Sink, ready: Ready, stop: &Stop) -> Result<(), String> {
    let c = match content() {
        Ok(c) => c,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    let main = CGMainDisplayID();
    let displays: Vec<Retained<SCDisplay>> = unsafe { c.displays() }.to_vec();
    let scale = displays.iter().find(|d| unsafe { d.displayID() } == main).map_or(2.0, |d| {
        let (pw, _) = pixel_size(d);
        pw as f64 / unsafe { d.width() }.max(1) as f64
    });
    let window = id.strip_prefix("window:").and_then(|s| s.parse::<u32>().ok());
    // Content filter and its size in pixels.
    let target = if let Some(wid) = window {
        let windows: Vec<Retained<SCWindow>> = unsafe { c.windows() }.to_vec();
        windows.into_iter().find(|w| unsafe { w.windowID() } == wid).map(|w| unsafe {
            let f = w.frame();
            let size = ((f.size.width * scale) as u32, (f.size.height * scale) as u32);
            (SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w), size)
        })
    } else {
        let did = id.strip_prefix("display:").and_then(|s| s.parse::<u32>().ok());
        displays
            .iter()
            .find(|d| Some(unsafe { d.displayID() }) == did || (id.is_empty() && unsafe { d.displayID() } == main))
            .map(|d| unsafe {
                let empty = NSArray::<SCWindow>::new();
                (SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), d, &empty), pixel_size(d))
            })
    };
    let Some((filter, (pw, ph))) = target else {
        ready(Err("that display or window is no longer available".into()));
        return Ok(());
    };
    let (tw, th) = fit_size(pw, ph, config.max_width, config.max_height);
    let stopped = Arc::new(Mutex::new(None));
    let delegate = ScreenDelegate::new(ScreenIvars {
        handler: FrameHandler::new(sink, config.fps, config.max_width, config.max_height),
        stopped: stopped.clone(),
    });
    let queue = dispatch2::DispatchQueue::new("celerity.screen", None);
    let cfg = unsafe { SCStreamConfiguration::new() };
    let stream = unsafe {
        cfg.setWidth(tw as usize);
        cfg.setHeight(th as usize);
        cfg.setMinimumFrameInterval(CMTime::new(1, config.fps.clamp(1, 60) as i32));
        cfg.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        cfg.setShowsCursor(config.show_cursor);
        cfg.setQueueDepth(3);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &cfg,
            Some(ProtocolObject::from_ref(&*delegate)),
        );
        if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*delegate),
            SCStreamOutputType::Screen,
            Some(&queue),
        ) {
            ready(Err(format!("screen capture failed: {}", e.localizedDescription())));
            return Ok(());
        }
        stream
    };
    let (tx, rx) = mpsc::channel();
    let started = RcBlock::new(move |error: *mut NSError| {
        let r = match unsafe { error.as_ref() } {
            None => Ok(()),
            Some(e) => Err(e.localizedDescription().to_string()),
        };
        let _ = tx.send(r);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => ready(Ok(())),
        Ok(Err(e)) => {
            ready(Err(format!("screen capture failed to start: {e}")));
            return Ok(());
        }
        Err(_) => {
            ready(Err("screen capture did not start".into()));
            return Ok(());
        }
    }
    let mut result = Ok(());
    let mut size = (pw, ph);
    let mut checked = Instant::now();
    while !stop.sleep(Duration::from_millis(100)) {
        if let Some(e) = stopped.lock().unwrap_or_else(|e| e.into_inner()).take() {
            result = Err(e);
            break;
        }
        // A shared window can close (ScreenCaptureKit then just goes quiet) or change size
        // (the stream keeps its configured size and would distort the picture).
        if let Some(wid) = window
            && checked.elapsed() >= Duration::from_millis(500)
        {
            checked = Instant::now();
            let Some((w, h)) = window_points(wid) else {
                result = Err("the window was closed".into());
                break;
            };
            let now = ((w * scale) as u32, (h * scale) as u32);
            if now != size && now.0 >= 2 && now.1 >= 2 {
                size = now;
                let (tw, th) = fit_size(now.0, now.1, config.max_width, config.max_height);
                unsafe {
                    cfg.setWidth(tw as usize);
                    cfg.setHeight(th as usize);
                    stream.updateConfiguration_completionHandler(&cfg, None);
                }
            }
        }
    }
    let (tx, rx) = mpsc::channel::<()>();
    let done = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&done)) };
    let _ = rx.recv_timeout(Duration::from_secs(2));
    result
}

/// Size in points of window `wid`; `None` once it is gone.
fn window_points(wid: u32) -> Option<(f64, f64)> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, wid)?;
    if list.is_empty() {
        return None;
    }
    // SAFETY: the window list holds one CFDictionary per window.
    let info = unsafe { list.cast_unchecked::<CFDictionary>() }.get(0)?;
    let key: &CFString = unsafe { kCGWindowBounds };
    let bounds = unsafe { info.value((key as *const CFString).cast()) };
    let mut rect = CGRect::default();
    // SAFETY: kCGWindowBounds holds a CGRect dictionary representation.
    let ok = !bounds.is_null()
        && unsafe { CGRectMakeWithDictionaryRepresentation(Some(&*(bounds as *const CFDictionary)), &mut rect) };
    // A window whose bounds cannot be read still exists: report no change.
    Some(if ok { (rect.size.width, rect.size.height) } else { (0.0, 0.0) })
}
