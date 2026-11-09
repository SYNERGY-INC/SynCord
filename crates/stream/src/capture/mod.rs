//! Camera and screen capture. Every capture runs on its own thread and hands each frame, already
//! converted to I420 at the requested size, to a callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::frame::I420Frame;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;
#[cfg(windows)]
pub(crate) use self::windows::window_pid;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(crate) fn list_cameras() -> Result<Vec<DeviceInfo>, String> {
        Ok(Vec::new())
    }
    pub(crate) fn list_screens() -> Result<Vec<ScreenInfo>, String> {
        Ok(Vec::new())
    }
    pub(crate) fn run_camera(_: &str, _: CameraConfig, _: Sink, ready: Ready, _: &Stop) -> Result<(), String> {
        ready(Err("camera capture is not supported on this platform".into()));
        Ok(())
    }
    pub(crate) fn run_screen(_: &str, _: ScreenConfig, _: Sink, ready: Ready, _: &Stop) -> Result<(), String> {
        ready(Err("screen capture is not supported on this platform".into()));
        Ok(())
    }
}

/// A camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable identifier to pass to [`Capture::camera`] (device path / symbolic link / unique id).
    pub id: String,
    /// Human-readable name, e.g. "Integrated Webcam".
    pub name: String,
}

/// What a [`ScreenInfo`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenKind {
    Display,
    Window,
}

/// A display (or window) that can be shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenInfo {
    /// Identifier to pass to [`Capture::screen`].
    pub id: String,
    /// Human-readable name, e.g. "Display 1 (DELL U2719D)" or a window title.
    pub name: String,
    pub kind: ScreenKind,
    /// Size in physical pixels.
    pub width: u32,
    pub height: u32,
    /// The primary display.
    pub primary: bool,
}

/// Camera request. The camera mode closest to (and preferably not below) this is opened, and
/// frames are scaled to fit `width`x`height` keeping the aspect ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        CameraConfig { width: 640, height: 360, fps: 30 }
    }
}

/// Screen capture request: frames are scaled down (never up) to fit `max_width`x`max_height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenConfig {
    pub max_width: u32,
    pub max_height: u32,
    /// Target frame rate, 1..=60 (5-30 is sensible; frames are only taken when the screen
    /// changed where the platform reports that).
    pub fps: u32,
    /// Draw the mouse pointer into the frames.
    pub show_cursor: bool,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        ScreenConfig { max_width: 1920, max_height: 1080, fps: 15, show_cursor: true }
    }
}

/// Lists cameras. An empty list (not an error) when there are none.
pub fn list_cameras() -> Result<Vec<DeviceInfo>, String> {
    platform::list_cameras()
}

/// Lists displays (and on macOS, windows) that can be captured.
pub fn list_screens() -> Result<Vec<ScreenInfo>, String> {
    platform::list_screens()
}

pub(crate) type Sink = Box<dyn FnMut(&I420Frame) + Send + 'static>;
/// Called once by a backend: `Ok` when frames are about to flow, `Err` if opening failed.
/// Backends (`run_camera` / `run_screen`) call it, then loop until [`Stop`] is set, and return
/// `Err` if the capture ended on its own after starting (unplugged, display gone).
pub(crate) type Ready = Box<dyn FnOnce(Result<(), String>) + Send + 'static>;

/// Stop flag shared with a capture thread.
pub(crate) struct Stop(AtomicBool);

impl Stop {
    pub(crate) fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Sleeps up to `d`, returning early (with `true`) if stopped.
    #[allow(dead_code)]
    pub(crate) fn sleep(&self, d: Duration) -> bool {
        let step = Duration::from_millis(20);
        let mut left = d;
        while !left.is_zero() {
            if self.stopped() {
                return true;
            }
            let s = left.min(step);
            std::thread::sleep(s);
            left -= s;
        }
        self.stopped()
    }
}

/// A running capture. Dropping it stops the capture and joins its thread.
pub struct Capture {
    stop: Arc<Stop>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens camera `id` (from [`list_cameras`]) and calls `sink` for every frame on the capture
    /// thread. Returns once the camera is streaming, or with the reason it could not be opened
    /// (busy, unplugged, permission denied...).
    pub fn camera(
        id: &str,
        config: CameraConfig,
        sink: impl FnMut(&I420Frame) + Send + 'static,
    ) -> Result<Capture, String> {
        let id = id.to_string();
        Capture::spawn("celerity-camera", Box::new(sink), move |sink, ready, stop| {
            platform::run_camera(&id, config, sink, ready, stop)
        })
    }

    /// Starts capturing display (or window) `id` (from [`list_screens`]); see [`Capture::camera`].
    pub fn screen(
        id: &str,
        config: ScreenConfig,
        sink: impl FnMut(&I420Frame) + Send + 'static,
    ) -> Result<Capture, String> {
        let id = id.to_string();
        let config = ScreenConfig { fps: config.fps.clamp(1, 60), ..config };
        Capture::spawn("celerity-screen", Box::new(sink), move |sink, ready, stop| {
            platform::run_screen(&id, config, sink, ready, stop)
        })
    }

    /// Runs a backend on its own thread and waits until it reports readiness. Also used for
    /// audio capture.
    pub(crate) fn spawn<S: Send + 'static>(
        name: &str,
        sink: S,
        run: impl FnOnce(S, Ready, &Stop) -> Result<(), String> + Send + 'static,
    ) -> Result<Capture, String> {
        let stop = Arc::new(Stop(AtomicBool::new(false)));
        let error = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let (stop2, error2) = (stop.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let tx2 = tx.clone();
                let ready: Ready = Box::new(move |r| {
                    let _ = tx2.send(r);
                });
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(sink, ready, &stop2)));
                let msg = match result {
                    Ok(Ok(())) => "capture stopped".to_string(),
                    Ok(Err(e)) => e,
                    Err(_) => "capture thread crashed".to_string(),
                };
                // If the backend never reported readiness this reaches `Capture::spawn`.
                let _ = tx.send(Err(msg.clone()));
                if !stop2.stopped() {
                    let mut e = error2.lock().unwrap_or_else(|e| e.into_inner());
                    e.get_or_insert(msg);
                }
            })
            .map_err(|e| format!("could not start capture thread: {e}"))?;
        let mut cap = Capture { stop, error, thread: Some(thread) };
        match rx.recv_timeout(Duration::from_secs(130)) {
            Ok(Ok(())) => Ok(cap),
            Ok(Err(e)) => {
                cap.stop.0.store(true, Ordering::Relaxed);
                Err(e)
            }
            Err(_) => {
                cap.stop.0.store(true, Ordering::Relaxed);
                // Do not join a thread that is stuck in a driver call.
                cap.thread.take();
                Err("timed out opening the capture device".into())
            }
        }
    }

    /// Set when the capture ended on its own (device unplugged, display gone, error).
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `false` once the capture thread has ended.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.0.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Backends notice the flag within ~100 ms. A thread stuck inside a driver call is
            // left behind rather than freezing the caller (usually the UI thread).
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// A camera mode offered by a device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Mode {
    /// Backend-specific format id.
    pub format: u32,
    /// Preference among formats at the same size and rate (lower is better: raw before MJPEG).
    pub rank: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

/// Picks the mode that best serves the request: frame rate first, then the smallest size that
/// covers the request (or the largest below it), then the cheapest format.
pub(crate) fn choose_mode(modes: &[Mode], want: CameraConfig) -> Option<Mode> {
    let want_fps = want.fps.max(1) as f32 * 0.9;
    modes.iter().copied().min_by_key(|m| {
        let fps_ok = m.fps >= want_fps.min(15.0) && (m.fps >= want_fps || m.fps >= 30.0);
        let covers = m.width >= want.width && m.height >= want.height;
        let area = m.width as i64 * m.height as i64;
        let size_key = if covers { area } else { i64::MAX / 2 - area };
        (!fps_ok, size_key, m.rank, -(m.fps as i64))
    })
}

/// Frame pacing for backends that poll: lets frames through at most `fps` per second.
#[allow(dead_code)]
pub(crate) struct Pacer {
    interval_us: u64,
    next_us: u64,
}

#[allow(dead_code)]
impl Pacer {
    pub(crate) fn new(fps: u32) -> Pacer {
        Pacer { interval_us: 1_000_000 / fps.clamp(1, 120) as u64, next_us: 0 }
    }

    /// `true` if a frame captured at `now_us` should be used.
    pub(crate) fn accept(&mut self, now_us: u64) -> bool {
        // A quarter interval of slack so a source running at exactly `fps` is not halved by jitter.
        if now_us + self.interval_us / 4 < self.next_us {
            return false;
        }
        self.next_us = (self.next_us + self.interval_us).max(now_us);
        true
    }

    /// How long to wait from `now_us` until the next frame is due.
    pub(crate) fn wait(&self, now_us: u64) -> Duration {
        Duration::from_micros(self.next_us.saturating_sub(now_us))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_halves_30_to_15() {
        let mut p = Pacer::new(15);
        let accepted = (0..30u64).filter(|i| p.accept(i * 33_333)).count();
        assert_eq!(accepted, 15);
        let mut p = Pacer::new(30);
        let accepted = (0..30u64).filter(|i| p.accept(i * 33_333 + (i % 3) * 3000)).count();
        assert_eq!(accepted, 30);
    }

    #[test]
    fn enumeration_does_not_fail_without_devices() {
        // CI machines have no camera; listing must still work (empty or not).
        let cams = list_cameras();
        assert!(cams.is_ok(), "{cams:?}");
        // Screens may legitimately error without a display server; must not panic.
        let _ = list_screens();
    }

    #[test]
    fn opening_a_missing_camera_fails_cleanly() {
        let r = Capture::camera("no-such-camera", CameraConfig::default(), |_| {});
        assert!(r.is_err());
    }
}
