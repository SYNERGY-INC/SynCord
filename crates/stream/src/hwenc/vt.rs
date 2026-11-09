//! VideoToolbox H.264 encoding (Apple's media engine): a real-time compression session without
//! frame reordering, flushed after every frame so each frame's output is available at once.
//! Output comes as length-prefixed NAL units (AVCC) with parameter sets in the format
//! description; it is rewritten to Annex B with SPS/PPS in front of every IDR.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::*;

use super::{Au, Backend, Candidate, ParamSets, nal};
use crate::frame::I420Frame;
use crate::h264::EncoderConfig;

pub(crate) fn candidates() -> Vec<Candidate> {
    // Low-latency rate control (macOS 11.3+) first: it is what real-time apps use and keeps
    // frame sizes even; the classic rate control as a fallback.
    vec![
        Candidate {
            name: "videotoolbox",
            open: Box::new(|cfg, w, h| VtEncoder::open(cfg, w, h, true, true).map(|e| Box::new(e) as Box<dyn Backend>)),
        },
        Candidate {
            name: "videotoolbox",
            open: Box::new(|cfg, w, h| {
                VtEncoder::open(cfg, w, h, true, false).map(|e| Box::new(e) as Box<dyn Backend>)
            }),
        },
    ]
}

/// Output of the compression callback, collected until `encode` returns.
#[derive(Default)]
struct Outputs {
    aus: Vec<Au>,
    params: ParamSets,
    error: Option<i32>,
}

struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Boxed so the callback's pointer stays valid; freed after the session is invalidated.
    outputs: Box<Mutex<Outputs>>,
    width: u32,
    height: u32,
    fps: f32,
    bitrate: u32,
}

// The session is used from the encoder thread only; VideoToolbox itself is thread safe.
unsafe impl Send for VtEncoder {}

fn cfnum(v: i64) -> CFRetained<CFNumber> {
    CFNumber::new_i64(v)
}

unsafe fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> bool {
    unsafe { VTSessionSetProperty(session, key, Some(value)) == 0 }
}

/// Copies one NAL unit stream in AVCC form (`len_size`-byte big-endian lengths) to `units`.
fn split_avcc(data: &[u8], len_size: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + len_size <= data.len() {
        let n = data[i..i + len_size].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        i += len_size;
        if n == 0 || i + n > data.len() {
            break;
        }
        out.push(&data[i..i + n]);
        i += n;
    }
    out
}

unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the boxed `Mutex<Outputs>` of a live encoder (the session is
    // invalidated, which waits for callbacks, before the box is freed).
    let outputs = unsafe { &*(refcon as *const Mutex<Outputs>) };
    let mut o = outputs.lock().unwrap_or_else(|e| e.into_inner());
    if status != 0 {
        o.error = Some(status);
        return;
    }
    let Some(sample) = (unsafe { sample.as_ref() }) else { return }; // dropped frame
    unsafe {
        let ts = sample.presentation_time_stamp();
        let ts_ms = if ts.timescale > 0 { (ts.value.max(0) as i128 * 1000 / ts.timescale as i128) as u64 } else { 0 };
        // Parameter sets and the NAL length size from the format description.
        let mut len_size = 4usize;
        if let Some(desc) = sample.format_description() {
            let (sps, pps, n) = param_sets(&desc);
            if let (Some(sps), Some(pps)) = (sps, pps) {
                o.params.set(sps, pps);
            }
            len_size = n.clamp(1, 4);
        }
        let Some(block) = sample.data_buffer() else { return };
        let len = block.data_length();
        let mut data = vec![0u8; len];
        if len == 0 || block.copy_data_bytes(0, len, NonNull::new_unchecked(data.as_mut_ptr().cast())) != 0 {
            return;
        }
        let mut au = Au { data: Vec::with_capacity(len + 64), keyframe: false, ts_ms };
        let o = &mut *o;
        au.keyframe = o.params.normalize(split_avcc(&data, len_size), &mut au.data);
        if !au.data.is_empty() {
            o.aus.push(au);
        }
    }
}

/// SPS, PPS and the AVCC length size of an H.264 format description.
unsafe fn param_sets(desc: &CMFormatDescription) -> (Option<&[u8]>, Option<&[u8]>, usize) {
    let (mut sps, mut pps, mut len_size) = (None, None, 4usize);
    for i in 0..2 {
        let (mut p, mut size, mut count, mut header) = (std::ptr::null::<u8>(), 0usize, 0usize, 0i32);
        let r = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, i, &mut p, &mut size, &mut count, &mut header)
        };
        if r != 0 || p.is_null() || size == 0 {
            continue;
        }
        len_size = header.max(1) as usize;
        let s = unsafe { std::slice::from_raw_parts(p, size) };
        match s[0] & 0x1f {
            nal::SPS => sps = Some(s),
            nal::PPS => pps = Some(s),
            _ => {}
        }
    }
    (sps, pps, len_size)
}

impl VtEncoder {
    fn open(cfg: &EncoderConfig, w: u32, h: u32, require_hw: bool, low_latency: bool) -> Result<VtEncoder, String> {
        unsafe {
            let yes: &CFType = CFBoolean::new(true);
            let mut spec_keys: Vec<&CFString> =
                vec![kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder];
            let mut spec_vals: Vec<&CFType> = vec![yes];
            if require_hw {
                spec_keys.push(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder);
                spec_vals.push(yes);
            }
            if low_latency {
                spec_keys.push(kVTVideoEncoderSpecification_EnableLowLatencyRateControl);
                spec_vals.push(yes);
            }
            let spec = CFDictionary::<CFString, CFType>::from_slices(&spec_keys, &spec_vals);
            // NV12 input buffers at the frame size (the session's pool hands them out).
            let (fmt, wn, hn) =
                (cfnum(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i64), cfnum(w as i64), cfnum(h as i64));
            let attrs = CFDictionary::<CFString, CFType>::from_slices(
                &[kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey],
                &[&**fmt, &**wn, &**hn],
            );
            let outputs = Box::new(Mutex::new(Outputs::default()));
            let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
            let status = VTCompressionSession::create(
                None,
                w as i32,
                h as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_ref()),
                Some(attrs.as_ref()),
                None,
                Some(on_output),
                (&raw const *outputs).cast_mut().cast(),
                NonNull::from(&mut raw),
            );
            let session = NonNull::new(raw)
                .filter(|_| status == 0)
                .map(|p| CFRetained::from_raw(p))
                .ok_or_else(|| format!("no H.264 encoder session (status {status})"))?;
            let enc = VtEncoder { session, outputs, width: w, height: h, fps: cfg.max_fps, bitrate: cfg.bitrate_bps };
            let s = &*enc.session;
            if !set(s, kVTCompressionPropertyKey_RealTime, yes) {
                return Err("real-time mode refused".into());
            }
            if !set(s, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false)) {
                return Err("cannot turn off frame reordering".into());
            }
            // Constrained High (no B-frames by definition) where the encoder offers it; OpenH264
            // decodes CABAC and 8x8 transforms.
            let profiles = [
                kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
                kVTProfileLevel_H264_High_AutoLevel,
                kVTProfileLevel_H264_Main_AutoLevel,
            ];
            if !profiles.iter().any(|p| set(s, kVTCompressionPropertyKey_ProfileLevel, p)) {
                return Err("no usable H.264 profile".into());
            }
            // Keyframes are placed by us; the encoder's own interval is only a backstop.
            let gop = (cfg.keyframe_interval.as_secs_f32() * cfg.max_fps.max(1.0)).max(1.0) as i64;
            set(s, kVTCompressionPropertyKey_MaxKeyFrameInterval, &cfnum(gop.max(30)));
            set(s, kVTCompressionPropertyKey_ExpectedFrameRate, &cfnum(cfg.max_fps.round() as i64));
            set(s, kVTCompressionPropertyKey_MaxFrameDelayCount, &cfnum(0));
            enc.apply_bitrate();
            let _ = s.prepare_to_encode_frames();
            Ok(enc)
        }
    }

    /// Average bitrate plus a hard cap at 1.5x over one second (keyframes may burst within it).
    fn apply_bitrate(&self) {
        unsafe {
            let s = &*self.session;
            set(s, kVTCompressionPropertyKey_AverageBitRate, &cfnum(self.bitrate as i64));
            let bytes = cfnum(self.bitrate as i64 * 3 / 2 / 8);
            let secs = CFNumber::new_f64(1.0);
            let limits = objc2_core_foundation::CFArray::<CFNumber>::from_objects(&[&*bytes, &*secs]);
            set(s, kVTCompressionPropertyKey_DataRateLimits, limits.as_ref());
        }
    }

    unsafe fn pixel_buffer(&self, frame: &I420Frame) -> Result<CFRetained<CVPixelBuffer>, String> {
        unsafe {
            let pool: CFRetained<CVPixelBufferPool> =
                self.session.pixel_buffer_pool().ok_or("the encoder has no pixel buffer pool")?;
            let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
            let r = CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw));
            let pb = NonNull::new(raw)
                .filter(|_| r == 0)
                .map(|p| CFRetained::from_raw(p))
                .ok_or_else(|| format!("no pixel buffer ({r})"))?;
            if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty()) != 0 {
                return Err("cannot lock the pixel buffer".into());
            }
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let y = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
            let uv = CVPixelBufferGetBaseAddressOfPlane(&pb, 1) as *mut u8;
            let (ys, uvs) = (CVPixelBufferGetBytesPerRowOfPlane(&pb, 0), CVPixelBufferGetBytesPerRowOfPlane(&pb, 1));
            if !y.is_null() && !uv.is_null() && ys >= w && uvs >= w {
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(frame.y()[row * w..].as_ptr(), y.add(row * ys), w);
                }
                let (u, v) = (frame.u(), frame.v());
                for row in 0..h / 2 {
                    let dst = std::slice::from_raw_parts_mut(uv.add(row * uvs), w);
                    let (ur, vr) = (&u[row * w / 2..][..w / 2], &v[row * w / 2..][..w / 2]);
                    for ((d, &a), &b) in dst.as_chunks_mut::<2>().0.iter_mut().zip(ur).zip(vr) {
                        *d = [a, b];
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            Ok(pb)
        }
    }
}

impl Backend for VtEncoder {
    fn name(&self) -> &'static str {
        "videotoolbox"
    }

    fn encode(&mut self, frame: &I420Frame, ts_ms: u64, key: bool, out: &mut Vec<Au>) -> Result<(), String> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err("frame size changed".into());
        }
        unsafe {
            let pb = self.pixel_buffer(frame)?;
            let pts = CMTime::new(ts_ms as i64, 1000);
            let duration = CMTime::new(1000 / self.fps.max(1.0) as i64, 1000);
            let props = key.then(|| {
                CFDictionary::<CFString, CFType>::from_slices(
                    &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                    &[CFBoolean::new(true) as &CFType],
                )
            });
            let r = self.session.encode_frame(
                &pb,
                pts,
                duration,
                props.as_deref().map(|d| d.as_ref()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != 0 {
                return Err(format!("encoding failed ({r})"));
            }
            // Real time, no reordering: completing up to this frame returns its output now.
            let r = self.session.complete_frames(pts);
            if r != 0 {
                return Err(format!("encoding failed ({r})"));
            }
        }
        let mut o = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = o.error.take() {
            return Err(format!("encoding failed ({e})"));
        }
        out.append(&mut o.aus);
        Ok(())
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.apply_bitrate();
    }

    fn set_max_fps(&mut self, fps: f32) {
        self.fps = fps.clamp(1.0, 60.0);
        unsafe {
            set(&self.session, kVTCompressionPropertyKey_ExpectedFrameRate, &cfnum(self.fps.round() as i64));
        }
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // Waits for pending callbacks; after this `outputs` is no longer referenced.
        unsafe { self.session.invalidate() };
    }
}

/// VideoToolbox output must decode with OpenH264 (what every receiver uses). Runs on GitHub's
/// Apple Silicon runners (`cargo test --release -p celerity-stream`); virtual machines may lack
/// the hardware encoder, so Apple's software H.264 encoder is tested then.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ContentKind;
    use crate::hwenc::{self_test, split_annexb};

    fn config() -> EncoderConfig {
        EncoderConfig { content: ContentKind::Screen, bitrate_bps: 4_000_000, max_fps: 30.0, ..EncoderConfig::screen() }
    }

    fn any_session(w: u32, h: u32) -> VtEncoder {
        for (hw, ll) in [(true, true), (true, false), (false, false)] {
            match VtEncoder::open(&config(), w, h, hw, ll) {
                Ok(e) => {
                    eprintln!("VideoToolbox session: hardware required {hw}, low latency {ll}");
                    return e;
                }
                Err(e) => eprintln!("hardware {hw} low latency {ll}: {e}"),
            }
        }
        panic!("no VideoToolbox H.264 encoder at all");
    }

    #[test]
    fn videotoolbox_output_decodes_with_openh264() {
        let mut enc = any_session(1280, 720);
        self_test(&mut enc, 1280, 720, 12, 1.0).expect("self-test");
        let mut out = Vec::new();
        let mut f = I420Frame::new(1280, 720);
        let mut keys = Vec::new();
        for n in 0..20u64 {
            crate::test_pattern::render_test_pattern(n, &mut f);
            if n == 10 {
                enc.set_bitrate(1_000_000);
                enc.set_max_fps(15.0);
            }
            enc.encode(&f, 1000 + n * 33, n == 0 || n == 15, &mut out).unwrap();
            for au in out.drain(..) {
                if au.keyframe {
                    keys.push(au.ts_ms);
                    let units = split_annexb(&au.data);
                    assert!(units.iter().any(|u| u[0] & 0x1f == 7) && units.iter().any(|u| u[0] & 0x1f == 8));
                }
            }
        }
        assert!(keys.contains(&(1000 + 15 * 33)), "forced keyframe missing: {keys:?}");
    }

    #[test]
    fn odd_sizes_and_4k() {
        for (w, h) in [(1366, 768), (3840, 2160)] {
            let mut enc = any_session(w, h);
            self_test(&mut enc, w, h, 6, 1.0).unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn avcc_split() {
        let data = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41];
        assert_eq!(split_avcc(&data, 4), vec![&[0x65, 1][..], &[0x41][..]]);
        assert_eq!(split_avcc(&[0, 0, 0, 9, 1], 4), Vec::<&[u8]>::new());
    }
}
