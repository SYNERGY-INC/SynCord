//! Windows: Media Foundation cameras, DXGI desktop duplication screens (GDI fallback),
//! Windows.Graphics.Capture windows.

mod camera;
mod screen;
mod window;

pub(crate) use camera::{list_cameras, run_camera};
pub(crate) use screen::{list_screens, run_screen};
pub(crate) use window::window_pid;

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// COM (multithreaded) + Media Foundation for the current thread; undone on drop.
pub(crate) struct ComMf {
    com: bool,
}

impl ComMf {
    pub(crate) fn init() -> Result<ComMf, String> {
        unsafe {
            // S_FALSE (already initialised the same way) is fine; RPC_E_CHANGED_MODE means the
            // thread is STA, where MF still works, so carry on without balancing it.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com = hr.is_ok();
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
                if com {
                    CoUninitialize();
                }
                return Err(format!(
                    "Media Foundation is not available (Windows N edition without the Media Feature Pack?): {}",
                    e.message()
                ));
            }
            Ok(ComMf { com })
        }
    }
}

impl Drop for ComMf {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com {
                CoUninitialize();
            }
        }
    }
}
