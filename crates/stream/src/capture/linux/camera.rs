//! V4L2 camera capture with memory-mapped buffers, talking to the kernel directly through
//! `ioctl` (no libv4l, no bindgen). Formats: YUYV, UYVY, NV12, YU12 and MJPEG.

use std::ffi::CString;
use std::os::raw::{c_int, c_ulong, c_void};

use crate::capture::{CameraConfig, DeviceInfo, Mode, Pacer, Ready, Sink, Stop, choose_mode};
use crate::convert::{Converter, RawFrame};
use crate::frame::I420Frame;
use crate::now_us;

// ---------------------------------------------------------------------------------------------
// Kernel ABI (linux/videodev2.h), 64-bit layout.

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const FRMSIZE_DISCRETE: u32 = 1;
const FRMIVAL_DISCRETE: u32 = 1;

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
const PIX_YUYV: u32 = fourcc(b"YUYV");
const PIX_UYVY: u32 = fourcc(b"UYVY");
const PIX_NV12: u32 = fourcc(b"NV12");
const PIX_YU12: u32 = fourcc(b"YU12");
const PIX_MJPG: u32 = fourcc(b"MJPG");
const PIX_JPEG: u32 = fourcc(b"JPEG");

/// Preference among formats of the same size and rate (raw formats need no decoding).
fn format_rank(f: u32) -> Option<u32> {
    Some(match f {
        PIX_NV12 => 0,
        PIX_YUYV => 1,
        PIX_YU12 => 2,
        PIX_UYVY => 3,
        PIX_MJPG | PIX_JPEG => 4,
        _ => return None,
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    /// discrete: width, height; stepwise: min_w, max_w, step_w, min_h, max_h, step_h.
    u: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    /// discrete: numerator, denominator; stepwise: min, max, step fractions.
    u: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    // The kernel union contains pointers, so it is pointer aligned.
    _align: [usize; 200 / std::mem::size_of::<usize>()],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Format {
    type_: u32,
    fmt: FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: [u32; 2],
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union ParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamParm {
    type_: u32,
    parm: ParmUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    /// union { offset; userptr; planes; fd }
    m: usize,
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Capability>() == 104);
    assert!(std::mem::size_of::<FmtDesc>() == 64);
    assert!(std::mem::size_of::<FrmSizeEnum>() == 44);
    assert!(std::mem::size_of::<FrmIvalEnum>() == 52);
    assert!(std::mem::size_of::<Format>() == 208);
    assert!(std::mem::size_of::<StreamParm>() == 204);
    assert!(std::mem::size_of::<RequestBuffers>() == 20);
    assert!(std::mem::size_of::<Buffer>() == 88);
};

const fn ioc(dir: u32, nr: u32, size: usize) -> c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as c_ulong
}
const R: u32 = 2;
const W: u32 = 1;
const RW: u32 = 3;
const VIDIOC_QUERYCAP: c_ulong = ioc(R, 0, std::mem::size_of::<Capability>());
const VIDIOC_ENUM_FMT: c_ulong = ioc(RW, 2, std::mem::size_of::<FmtDesc>());
const VIDIOC_S_FMT: c_ulong = ioc(RW, 5, std::mem::size_of::<Format>());
const VIDIOC_REQBUFS: c_ulong = ioc(RW, 8, std::mem::size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: c_ulong = ioc(RW, 9, std::mem::size_of::<Buffer>());
const VIDIOC_QBUF: c_ulong = ioc(RW, 15, std::mem::size_of::<Buffer>());
const VIDIOC_DQBUF: c_ulong = ioc(RW, 17, std::mem::size_of::<Buffer>());
const VIDIOC_STREAMON: c_ulong = ioc(W, 18, std::mem::size_of::<c_int>());
const VIDIOC_STREAMOFF: c_ulong = ioc(W, 19, std::mem::size_of::<c_int>());
const VIDIOC_S_PARM: c_ulong = ioc(RW, 22, std::mem::size_of::<StreamParm>());
const VIDIOC_ENUM_FRAMESIZES: c_ulong = ioc(RW, 74, std::mem::size_of::<FrmSizeEnum>());
const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong = ioc(RW, 75, std::mem::size_of::<FrmIvalEnum>());

fn zeroed<T>() -> T {
    // All the structs above are plain integers / byte arrays, for which all-zero is valid.
    unsafe { std::mem::zeroed() }
}

/// An open device node, closed on drop.
struct Fd(c_int);

impl Fd {
    fn open(path: &str) -> Result<Fd, String> {
        let c = CString::new(path).map_err(|_| "bad device path".to_string())?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::EBUSY) => "the camera is in use by another application".into(),
                Some(libc::EACCES) => format!("no permission to open {path} (is the user in the 'video' group?)"),
                Some(libc::ENOENT) | Some(libc::ENODEV) => "camera not found".into(),
                _ => format!("cannot open {path}: {e}"),
            });
        }
        Ok(Fd(fd))
    }

    /// `ioctl`, retried on EINTR.
    fn ioctl<T>(&self, req: c_ulong, arg: &mut T) -> std::io::Result<()> {
        loop {
            let r = unsafe { libc::ioctl(self.0, req as _, arg as *mut T as *mut c_void) };
            if r != -1 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    fn capability(&self) -> Option<Capability> {
        let mut cap: Capability = zeroed();
        self.ioctl(VIDIOC_QUERYCAP, &mut cap).ok()?;
        Some(cap)
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn is_capture_device(cap: &Capability) -> bool {
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
    caps & CAP_VIDEO_CAPTURE != 0 && caps & CAP_STREAMING != 0
}

fn video_nodes() -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev")
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let n = name.strip_prefix("video")?.parse::<u32>().ok()?;
                    Some((n, format!("/dev/{name}")))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes.into_iter().map(|(_, p)| p).collect()
}

pub(crate) fn list_cameras() -> Result<Vec<DeviceInfo>, String> {
    let mut out: Vec<DeviceInfo> = Vec::new();
    for path in video_nodes() {
        // UVC cameras expose a second (metadata) node; only video capture nodes are cameras.
        let Ok(fd) = Fd::open(&path) else { continue };
        let Some(cap) = fd.capability() else { continue };
        if !is_capture_device(&cap) || pick_modes(&fd).is_empty() {
            continue;
        }
        let mut name = cstr(&cap.card);
        if name.is_empty() {
            name = path.clone();
        }
        out.push(DeviceInfo { id: path, name });
    }
    // Same model twice: tell them apart.
    let names: Vec<String> = out.iter().map(|d| d.name.clone()).collect();
    for d in &mut out {
        if names.iter().filter(|n| **n == d.name).count() > 1 {
            d.name = format!("{} ({})", d.name, d.id.trim_start_matches("/dev/"));
        }
    }
    Ok(out)
}

/// All supported (format, size, best fps) combinations of a device.
fn pick_modes(fd: &Fd) -> Vec<Mode> {
    let mut modes = Vec::new();
    for i in 0..64 {
        let mut d: FmtDesc = zeroed();
        d.index = i;
        d.type_ = BUF_TYPE_VIDEO_CAPTURE;
        if fd.ioctl(VIDIOC_ENUM_FMT, &mut d).is_err() {
            break;
        }
        if format_rank(d.pixelformat).is_none() {
            continue;
        }
        let mut sizes = Vec::new();
        for j in 0..256 {
            let mut s: FrmSizeEnum = zeroed();
            s.index = j;
            s.pixel_format = d.pixelformat;
            if fd.ioctl(VIDIOC_ENUM_FRAMESIZES, &mut s).is_err() {
                break;
            }
            if s.type_ == FRMSIZE_DISCRETE {
                sizes.push((s.u[0], s.u[1]));
            } else {
                // Stepwise / continuous: offer common sizes inside the range.
                let (minw, maxw, minh, maxh) = (s.u[0], s.u[1], s.u[3], s.u[4]);
                for (w, h) in [(640, 360), (640, 480), (1280, 720), (1920, 1080)] {
                    if (minw..=maxw).contains(&w) && (minh..=maxh).contains(&h) {
                        sizes.push((w, h));
                    }
                }
                sizes.push((maxw.min(1920), maxh.min(1080)));
                break;
            }
        }
        for (w, h) in sizes {
            let mut best = 0f32;
            for k in 0..64 {
                let mut iv: FrmIvalEnum = zeroed();
                iv.index = k;
                iv.pixel_format = d.pixelformat;
                iv.width = w;
                iv.height = h;
                if fd.ioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut iv).is_err() {
                    break;
                }
                // Discrete: numerator/denominator. Stepwise: the minimum interval comes first.
                let (n, dd) = (iv.u[0], iv.u[1]);
                if n > 0 {
                    best = best.max(dd as f32 / n as f32);
                }
                if iv.type_ != FRMIVAL_DISCRETE {
                    break;
                }
            }
            if best == 0.0 {
                best = 30.0; // driver did not say
            }
            modes.push(Mode {
                format: d.pixelformat,
                rank: format_rank(d.pixelformat).unwrap_or(9),
                width: w,
                height: h,
                fps: best,
            });
        }
    }
    modes
}

struct Mapped {
    ptr: *mut c_void,
    len: usize,
}

pub(crate) fn run_camera(
    id: &str,
    config: CameraConfig,
    mut sink: Sink,
    ready: Ready,
    stop: &Stop,
) -> Result<(), String> {
    let opened = open_stream(id, config);
    let (fd, mode, stride, bufs) = match opened {
        Ok(v) => v,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    ready(Ok(()));
    let result = stream_loop(&fd, mode, stride, &bufs, config, &mut sink, stop);
    let mut t = BUF_TYPE_VIDEO_CAPTURE as c_int;
    let _ = fd.ioctl(VIDIOC_STREAMOFF, &mut t);
    for b in &bufs {
        unsafe { libc::munmap(b.ptr, b.len) };
    }
    result
}

fn open_stream(id: &str, config: CameraConfig) -> Result<(Fd, Mode, usize, Vec<Mapped>), String> {
    let path = if id.is_empty() { video_nodes().into_iter().next().ok_or("no camera found")? } else { id.to_string() };
    if !path.starts_with("/dev/video") {
        return Err("camera not found".into());
    }
    let fd = Fd::open(&path)?;
    let cap = fd.capability().ok_or("not a V4L2 device")?;
    if !is_capture_device(&cap) {
        return Err("not a video capture device".into());
    }
    let modes = pick_modes(&fd);
    let mode = choose_mode(&modes, config).ok_or("the camera offers no supported format")?;

    let mut f: Format = zeroed();
    f.type_ = BUF_TYPE_VIDEO_CAPTURE;
    f.fmt.pix =
        PixFormat { width: mode.width, height: mode.height, pixelformat: mode.format, field: FIELD_NONE, ..zeroed() };
    fd.ioctl(VIDIOC_S_FMT, &mut f).map_err(|e| format!("camera rejected the format: {e}"))?;
    let pix = unsafe { f.fmt.pix };
    if format_rank(pix.pixelformat).is_none() || pix.width == 0 || pix.height == 0 {
        return Err("camera switched to an unsupported format".into());
    }
    let mode = Mode { format: pix.pixelformat, width: pix.width, height: pix.height, ..mode };
    let stride = match pix.pixelformat {
        PIX_YUYV | PIX_UYVY => (pix.bytesperline as usize).max(pix.width as usize * 2),
        _ => (pix.bytesperline as usize).max(pix.width as usize),
    };

    // Frame rate (best effort: many drivers only support their listed rates).
    let mut p: StreamParm = zeroed();
    p.type_ = BUF_TYPE_VIDEO_CAPTURE;
    p.parm.capture =
        CaptureParm { timeperframe: [1, config.fps.clamp(1, 60).min(mode.fps.round() as u32).max(1)], ..zeroed() };
    let _ = fd.ioctl(VIDIOC_S_PARM, &mut p);

    let mut rb = RequestBuffers { count: 3, type_: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..zeroed() };
    fd.ioctl(VIDIOC_REQBUFS, &mut rb).map_err(|e| format!("camera buffer setup failed: {e}"))?;
    if rb.count == 0 {
        return Err("camera gave no buffers".into());
    }
    let mut bufs = Vec::new();
    let cleanup = |bufs: &Vec<Mapped>| {
        for b in bufs {
            unsafe { libc::munmap(b.ptr, b.len) };
        }
    };
    for i in 0..rb.count {
        let mut b: Buffer = zeroed();
        b.index = i;
        b.type_ = BUF_TYPE_VIDEO_CAPTURE;
        b.memory = MEMORY_MMAP;
        if let Err(e) = fd.ioctl(VIDIOC_QUERYBUF, &mut b) {
            cleanup(&bufs);
            return Err(format!("camera buffer query failed: {e}"));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                b.length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.0,
                b.m as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            cleanup(&bufs);
            return Err("camera buffer mapping failed".into());
        }
        bufs.push(Mapped { ptr, len: b.length as usize });
        if let Err(e) = fd.ioctl(VIDIOC_QBUF, &mut b) {
            cleanup(&bufs);
            return Err(format!("camera buffer queueing failed: {e}"));
        }
    }
    let mut t = BUF_TYPE_VIDEO_CAPTURE as c_int;
    if let Err(e) = fd.ioctl(VIDIOC_STREAMON, &mut t) {
        cleanup(&bufs);
        return Err(match e.raw_os_error() {
            Some(libc::EBUSY) => "the camera is in use by another application".into(),
            Some(libc::ENOSPC) => {
                "not enough USB bandwidth for the camera (try another port or a lower resolution)".into()
            }
            _ => format!("camera failed to start: {e}"),
        });
    }
    Ok((fd, mode, stride, bufs))
}

fn stream_loop(
    fd: &Fd,
    mode: Mode,
    stride: usize,
    bufs: &[Mapped],
    config: CameraConfig,
    sink: &mut Sink,
    stop: &Stop,
) -> Result<(), String> {
    let mut conv = Converter::default();
    let mut frame = I420Frame::default();
    let mut pacer = Pacer::new(config.fps);
    let mut jpeg = Vec::new();
    let mut idle_ms = 0u32;
    let (w, h) = (mode.width as usize, mode.height as usize);
    while !stop.stopped() {
        let mut pfd = libc::pollfd { fd: fd.0, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
        if r == 0 {
            idle_ms += 100;
            if idle_ms >= 5000 {
                return Err("the camera stopped sending frames".into());
            }
            continue;
        }
        if r < 0 {
            continue; // EINTR
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 && pfd.revents & libc::POLLIN == 0 {
            return Err("camera disconnected".into());
        }
        let mut b: Buffer = zeroed();
        b.type_ = BUF_TYPE_VIDEO_CAPTURE;
        b.memory = MEMORY_MMAP;
        if let Err(e) = fd.ioctl(VIDIOC_DQBUF, &mut b) {
            match e.raw_os_error() {
                Some(libc::EAGAIN) => continue,
                Some(libc::ENODEV) => return Err("camera disconnected".into()),
                _ => return Err(format!("camera error: {e}")),
            }
        }
        idle_ms = 0;
        let t = now_us();
        if let Some(m) = bufs.get(b.index as usize)
            && pacer.accept(t)
        {
            let data = unsafe { std::slice::from_raw_parts(m.ptr as *const u8, (b.bytesused as usize).min(m.len)) };
            if let Some(raw) = raw_frame(mode.format, data, w, h, stride, &mut jpeg) {
                conv.convert(&raw, config.width, config.height, &mut frame);
                frame.timestamp_us = t;
                sink(&frame);
            }
        }
        if let Err(e) = fd.ioctl(VIDIOC_QBUF, &mut b) {
            return Err(format!("camera error: {e}"));
        }
    }
    Ok(())
}

/// Interprets one captured buffer (decoding MJPEG into `jpeg`).
fn raw_frame<'a>(
    format: u32,
    data: &'a [u8],
    w: usize,
    h: usize,
    stride: usize,
    jpeg: &'a mut Vec<u8>,
) -> Option<RawFrame<'a>> {
    match format {
        PIX_YUYV if data.len() >= stride * (h - 1) + w * 2 => {
            Some(RawFrame::Yuyv { data, stride, width: w, height: h })
        }
        PIX_UYVY if data.len() >= stride * (h - 1) + w * 2 => {
            Some(RawFrame::Uyvy { data, stride, width: w, height: h })
        }
        PIX_NV12 if data.len() >= stride * h * 3 / 2 => {
            let (y, uv) = data.split_at(stride * h);
            Some(RawFrame::Nv12 { y, y_stride: stride, uv, uv_stride: stride, width: w, height: h })
        }
        PIX_YU12 if data.len() >= stride * h * 3 / 2 => {
            let (y, rest) = data.split_at(stride * h);
            let cs = stride / 2;
            let (u, v) = rest.split_at(cs * h.div_ceil(2));
            Some(RawFrame::I420 { y, u, v, y_stride: stride, uv_stride: cs, width: w, height: h })
        }
        PIX_MJPG | PIX_JPEG => decode_mjpeg(data, jpeg),
        _ => None,
    }
}

/// Decodes a (UVC) MJPEG frame to interleaved full-range YCbCr, skipping colour conversion.
pub(crate) fn decode_mjpeg<'a>(data: &[u8], out: &'a mut Vec<u8>) -> Option<RawFrame<'a>> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr).set_strict_mode(false);
    let mut d = JpegDecoder::new_with_options(std::io::Cursor::new(data), opts);
    d.decode_headers().ok()?;
    let (w, h) = d.dimensions()?;
    if d.output_colorspace() != Some(ColorSpace::YCbCr) {
        return None;
    }
    out.resize(d.output_buffer_size()?, 0);
    d.decode_into(out).ok()?;
    if out.len() < w * h * 3 {
        return None;
    }
    Some(RawFrame::YccFull { data: out, width: w, height: h })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_choice() {
        let m = |format, width, height, fps| Mode { format, rank: format_rank(format).unwrap(), width, height, fps };
        let modes = [
            m(PIX_YUYV, 640, 480, 30.0),
            m(PIX_YUYV, 1280, 720, 10.0),
            m(PIX_MJPG, 640, 480, 30.0),
            m(PIX_MJPG, 1280, 720, 30.0),
            m(PIX_MJPG, 1920, 1080, 30.0),
            m(PIX_YUYV, 320, 240, 30.0),
        ];
        let want = |width, height, fps| CameraConfig { width, height, fps };
        assert_eq!(choose_mode(&modes, want(640, 360, 30)), Some(modes[0]));
        assert_eq!(choose_mode(&modes, want(1280, 720, 30)), Some(modes[3]));
        // Only slow raw modes at 720p and a 15 fps request: raw 10 fps is too slow, MJPEG wins.
        assert_eq!(choose_mode(&modes, want(1280, 720, 15)), Some(modes[3]));
        // Request larger than anything: the largest.
        assert_eq!(choose_mode(&modes, want(3840, 2160, 30)), Some(modes[4]));
        assert_eq!(choose_mode(&[], want(640, 360, 30)), None);
        // 1080p: raw at 30 beats MJPEG at 30; a 60 fps request takes MJPEG at 60.
        let hd = [m(PIX_MJPG, 1920, 1080, 60.0), m(PIX_NV12, 1920, 1080, 30.0), m(PIX_YUYV, 1920, 1080, 5.0)];
        assert_eq!(choose_mode(&hd, want(1920, 1080, 30)), Some(hd[1]));
        assert_eq!(choose_mode(&hd, want(1920, 1080, 60)), Some(hd[0]));
    }

    /// A 16x16 baseline JPEG, solid colour (R=200, G=60, B=40), 4:2:0, made with ImageMagick.
    const RED_JPEG: &[u8] = include_bytes!("test_red.jpg");

    #[test]
    fn mjpeg_decodes_to_limited_range_i420() {
        let mut buf = Vec::new();
        let raw = decode_mjpeg(RED_JPEG, &mut buf).expect("decode");
        assert_eq!(raw.size(), (16, 16));
        let mut f = I420Frame::default();
        Converter::default().convert(&raw, 16, 16, &mut f);
        let mut rgba = Vec::new();
        crate::convert::to_rgba(&(&f).into(), 16, 16, &mut rgba);
        let px = &rgba[8 * 16 * 4 + 32..][..4];
        // JPEG uses BT.601 and we treat output as BT.709, so allow some hue error.
        assert!(
            (px[0] as i32 - 200).abs() < 30 && (px[1] as i32 - 60).abs() < 30 && (px[2] as i32 - 40).abs() < 30,
            "{px:?}"
        );
        assert!(decode_mjpeg(b"not a jpeg", &mut buf).is_none());
    }
}
