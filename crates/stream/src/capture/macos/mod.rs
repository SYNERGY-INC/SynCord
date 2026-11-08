//! macOS: AVFoundation cameras, ScreenCaptureKit displays and windows (macOS 12.3+).
//!
//! Both deliver `CMSampleBuffer`s on a serial dispatch queue; the system already scaled them to
//! our size and converted them to NV12, so we only repack to I420 there.

mod camera;
mod screen;

pub(crate) use camera::{list_cameras, run_camera};
pub(crate) use screen::{list_screens, run_screen};

use std::sync::Mutex;

use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

use crate::capture::{Pacer, Sink};
use crate::convert::{Converter, RawFrame};
use crate::frame::I420Frame;
use crate::now_us;

/// Sends a value that is only touched from one thread at a time across threads (Objective-C
/// objects that objc2 conservatively marks `!Send`).
pub(crate) struct AssertSend<T>(pub T);
unsafe impl<T> Send for AssertSend<T> {}

/// Converts delivered sample buffers and passes them to the sink (shared by both captures).
pub(crate) struct FrameHandler {
    state: Mutex<HandlerState>,
}

struct HandlerState {
    sink: Sink,
    conv: Converter,
    frame: I420Frame,
    pacer: Pacer,
    max_w: u32,
    max_h: u32,
}

impl FrameHandler {
    pub(crate) fn new(sink: Sink, fps: u32, max_w: u32, max_h: u32) -> FrameHandler {
        FrameHandler {
            state: Mutex::new(HandlerState {
                sink,
                conv: Converter::default(),
                frame: I420Frame::default(),
                pacer: Pacer::new(fps),
                max_w,
                max_h,
            }),
        }
    }

    pub(crate) fn handle(&self, sample: &CMSampleBuffer) {
        let t = now_us();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.pacer.accept(t) {
            return;
        }
        let HandlerState { sink, conv, frame, max_w, max_h, .. } = &mut *st;
        let ok = unsafe {
            with_pixels(sample, |raw| {
                conv.convert(&raw, *max_w, *max_h, frame);
            })
        };
        if ok {
            frame.timestamp_us = t;
            sink(frame);
        }
    }
}

/// Locks the pixel buffer of `sample` and hands its pixels to `f`. `false` if the sample has no
/// image (ScreenCaptureKit idle frames) or an unexpected format.
unsafe fn with_pixels(sample: &CMSampleBuffer, f: impl FnOnce(RawFrame<'_>)) -> bool {
    let Some(img) = (unsafe { sample.image_buffer() }) else { return false };
    let pb = &*img;
    if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return false;
    }
    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    let mut ok = false;
    if fmt == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange && CVPixelBufferGetPlaneCount(pb) >= 2 {
        let (y, uv) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBaseAddressOfPlane(pb, 1));
        let (ys, uvs) = (CVPixelBufferGetBytesPerRowOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
        let (yh, uvh) = (CVPixelBufferGetHeightOfPlane(pb, 0), CVPixelBufferGetHeightOfPlane(pb, 1));
        if !y.is_null() && !uv.is_null() && w >= 2 && h >= 2 {
            let y = unsafe { std::slice::from_raw_parts(y as *const u8, ys * yh) };
            let uv = unsafe { std::slice::from_raw_parts(uv as *const u8, uvs * uvh) };
            f(RawFrame::Nv12 { y, y_stride: ys, uv, uv_stride: uvs, width: w, height: h });
            ok = true;
        }
    } else if fmt == kCVPixelFormatType_32BGRA {
        let p = CVPixelBufferGetBaseAddress(pb);
        let stride = CVPixelBufferGetBytesPerRow(pb);
        if !p.is_null() && w >= 2 && h >= 2 {
            let data = unsafe { std::slice::from_raw_parts(p as *const u8, stride * h) };
            f(RawFrame::Bgra { data, stride, width: w, height: h });
            ok = true;
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    ok
}
