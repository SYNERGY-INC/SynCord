//! Media Foundation camera capture through a synchronous source reader. The reader's advanced
//! video processing decodes MJPEG and scales to the requested size, handing us NV12.

use std::ffi::c_void;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR};

use super::ComMf;
use crate::capture::{CameraConfig, DeviceInfo, Mode, Pacer, Ready, Sink, Stop, choose_mode};
use crate::convert::{Converter, RawFrame, fit_size};
use crate::frame::I420Frame;
use crate::now_us;

struct Device {
    activate: IMFActivate,
    name: String,
    link: String,
}

unsafe fn attr_string(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        a.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

fn devices() -> Result<Vec<Device>, String> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| e.message())?;
        let attrs = attrs.ok_or("MFCreateAttributes failed")?;
        attrs
            .SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
            .map_err(|e| e.message())?;
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFEnumDeviceSources(&attrs, &mut arr, &mut n).map_err(|e| format!("cannot list cameras: {}", e.message()))?;
        let mut out = Vec::new();
        if !arr.is_null() {
            for slot in std::slice::from_raw_parts_mut(arr, n as usize) {
                // Take ownership of each reference so the array itself can be freed.
                if let Some(activate) = slot.take() {
                    let name = attr_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                        .unwrap_or_else(|| "Camera".into());
                    let link = attr_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                        .unwrap_or_default();
                    out.push(Device { activate, name, link });
                }
            }
            CoTaskMemFree(Some(arr as *const c_void));
        }
        Ok(out)
    }
}

pub(crate) fn list_cameras() -> Result<Vec<DeviceInfo>, String> {
    // COM must be initialised for MTA, which the caller's (UI) thread may not allow: list on a
    // short-lived thread of our own.
    std::thread::spawn(|| -> Result<Vec<DeviceInfo>, String> {
        let _com = ComMf::init()?;
        let devs = devices()?;
        let mut out: Vec<DeviceInfo> = devs.into_iter().map(|d| DeviceInfo { id: d.link, name: d.name }).collect();
        let names: Vec<String> = out.iter().map(|d| d.name.clone()).collect();
        for (i, d) in out.iter_mut().enumerate() {
            if names.iter().filter(|n| **n == d.name).count() > 1 {
                d.name = format!("{} ({})", d.name, i + 1);
            }
        }
        Ok(out)
    })
    .join()
    .unwrap_or_else(|_| Err("camera enumeration crashed".into()))
}

fn subtype_rank(g: &GUID) -> Option<u32> {
    Some(if *g == MFVideoFormat_NV12 {
        0
    } else if *g == MFVideoFormat_YUY2 {
        1
    } else if *g == MFVideoFormat_I420 || *g == MFVideoFormat_IYUV {
        2
    } else if *g == MFVideoFormat_UYVY {
        3
    } else if *g == MFVideoFormat_MJPG {
        4
    } else if *g == MFVideoFormat_RGB32 {
        5
    } else {
        return None;
    })
}

fn split_u64(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Current output: size and stride of the NV12 frames the reader delivers.
struct OutFormat {
    width: u32,
    height: u32,
    stride: usize,
}

unsafe fn current_format(reader: &IMFSourceReader, stream: u32) -> Result<OutFormat, String> {
    unsafe {
        let mt = reader.GetCurrentMediaType(stream).map_err(|e| e.message())?;
        let (w, h) = split_u64(mt.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.message())?);
        let stride = mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| (s as i32).unsigned_abs() as usize).unwrap_or(0);
        Ok(OutFormat { width: w, height: h, stride: stride.max(w as usize) })
    }
}

pub(crate) fn run_camera(
    id: &str,
    config: CameraConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let com = match ComMf::init() {
        Ok(c) => c,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    let opened = unsafe { open(id, config) };
    let (source, reader, stream, mut fmt) = match opened {
        Ok(v) => v,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    ready(Ok(()));
    let result = unsafe { read_loop(&reader, stream, &mut fmt, config, &mut sink, stop) };
    drop(reader);
    unsafe {
        let _ = source.Shutdown();
    }
    drop(source);
    drop(com);
    result
}

unsafe fn open(id: &str, config: CameraConfig) -> Result<(IMFMediaSource, IMFSourceReader, u32, OutFormat), String> {
    unsafe {
        let devs = devices()?;
        let dev = devs
            .iter()
            .find(|d| d.link == id)
            .or_else(|| if id.is_empty() { devs.first() } else { None })
            .ok_or("camera not found (unplugged?)")?;
        let source: IMFMediaSource = dev.activate.ActivateObject().map_err(|e| {
            format!("cannot open the camera (in use by another app, or camera access is off in Windows privacy settings): {}", e.message())
        })?;
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2).map_err(|e| e.message())?;
        let attrs = attrs.ok_or("MFCreateAttributes failed")?;
        // Lets the reader insert the MJPEG decoder and the video processor (format + size).
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).map_err(|e| e.message())?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs).map_err(|e| {
            let _ = source.Shutdown();
            format!("cannot start the camera: {}", e.message())
        })?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        let _ = reader.SetStreamSelection(stream, true);

        // Native modes.
        let mut types = Vec::new();
        let mut modes = Vec::new();
        for i in 0..512 {
            let Ok(mt) = reader.GetNativeMediaType(stream, i) else { break };
            let Ok(sub) = mt.GetGUID(&MF_MT_SUBTYPE) else { continue };
            let Some(rank) = subtype_rank(&sub) else { continue };
            let Ok(size) = mt.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
            let (w, h) = split_u64(size);
            let fps = mt
                .GetUINT64(&MF_MT_FRAME_RATE)
                .map(split_u64)
                .map(|(n, d)| if d == 0 { 30.0 } else { n as f32 / d as f32 })
                .unwrap_or(30.0);
            modes.push(Mode { format: types.len() as u32, rank, width: w, height: h, fps });
            types.push(mt);
        }
        let mode = choose_mode(&modes, config).ok_or_else(|| {
            let _ = source.Shutdown();
            "the camera offers no supported video format".to_string()
        })?;
        let native = &types[mode.format as usize];
        reader
            .SetCurrentMediaType(stream, None, native)
            .map_err(|e| format!("camera rejected its own format: {}", e.message()))?;

        // Ask for NV12 at the target size; fall back to NV12 at the native size (we scale).
        let (tw, th) = fit_size(mode.width, mode.height, config.width, config.height);
        let mut ok = false;
        for size in [(tw, th), (mode.width & !1, mode.height & !1)] {
            let out = MFCreateMediaType().map_err(|e| e.message())?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.message())?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.message())?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((size.0 as u64) << 32) | size.1 as u64).map_err(|e| e.message())?;
            if let Ok(rate) = native.GetUINT64(&MF_MT_FRAME_RATE) {
                let _ = out.SetUINT64(&MF_MT_FRAME_RATE, rate);
            }
            let _ = out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let _ = out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1);
            if reader.SetCurrentMediaType(stream, None, &out).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            let _ = source.Shutdown();
            return Err("the camera cannot deliver a format we can read".into());
        }
        let fmt = current_format(&reader, stream)?;
        Ok((source, reader, stream, fmt))
    }
}

unsafe fn read_loop(
    reader: &IMFSourceReader,
    stream: u32,
    fmt: &mut OutFormat,
    config: CameraConfig,
    sink: &mut Sink,
    stop: &Stop,
) -> Result<(), String> {
    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    while !stop.stopped() {
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut index = 0u32;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader
                .ReadSample(stream, 0, Some(&mut index), Some(&mut flags), Some(&mut ts), Some(&mut sample))
                .map_err(|e| format!("camera stopped: {}", e.message()))?;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err("camera stopped (device error or unplugged)".into());
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err("camera stopped".into());
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            *fmt = unsafe { current_format(reader, stream)? };
        }
        let Some(sample) = sample else { continue };
        let t = now_us();
        if !pacer.accept(t) {
            continue;
        }
        let Ok(buf) = (unsafe { sample.ConvertToContiguousBuffer() }) else { continue };
        let (w, h) = (fmt.width as usize, fmt.height as usize);
        // Prefer the 2D view (true pitch); otherwise the default stride.
        let mut emit = |data: &[u8], pitch: usize| {
            let ylen = pitch * h;
            if w < 2 || h < 2 || data.len() < ylen + pitch * (h / 2) {
                return;
            }
            let (y, uv) = data.split_at(ylen);
            let raw = RawFrame::Nv12 { y, y_stride: pitch, uv, uv_stride: pitch, width: w, height: h };
            conv.convert(&raw, config.width, config.height, &mut frame);
            frame.timestamp_us = t;
            sink(&frame);
        };
        unsafe {
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                if b2.Lock2D(&mut p, &mut pitch).is_ok() {
                    if !p.is_null() && pitch > 0 {
                        let pitch = pitch as usize;
                        emit(std::slice::from_raw_parts(p, pitch * h * 3 / 2), pitch);
                    }
                    let _ = b2.Unlock2D();
                    continue;
                }
            }
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            if buf.Lock(&mut p, None, Some(&mut len)).is_ok() {
                if !p.is_null() {
                    emit(std::slice::from_raw_parts(p, len as usize), fmt.stride);
                }
                let _ = buf.Unlock();
            }
        }
    }
    Ok(())
}
