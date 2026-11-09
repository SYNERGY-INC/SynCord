//! Media Foundation H.264 encoders: the GPU vendor's hardware MFT (NVENC, AMF, Quick Sync)
//! when there is one, else Windows' own software encoder (`CLSID_MSH264EncoderMFT`).
//!
//! Hardware MFTs are asynchronous: they announce through their event generator when they want
//! input (`METransformNeedInput`) and when output is ready (`METransformHaveOutput`), and must be
//! unlocked first (`MF_TRANSFORM_ASYNC_UNLOCK`). The software MFT is synchronous (`ProcessInput`
//! then `ProcessOutput` until it needs more input). Both get system-memory NV12 frames.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface, PWSTR};

use super::{Au, Backend, Candidate, ParamSets, split_annexb};
use crate::frame::I420Frame;
use crate::h264::EncoderConfig;

/// COM (multithreaded) and Media Foundation for the encoder thread; undone on drop.
struct MfInit {
    com: bool,
}

impl MfInit {
    fn new() -> Result<MfInit, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com = hr.is_ok();
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                return Err(format!("COM: {}", hr.message()));
            }
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
                if com {
                    CoUninitialize();
                }
                return Err(format!("Media Foundation is not available: {}", e.message()));
            }
            Ok(MfInit { com })
        }
    }
}

impl Drop for MfInit {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com {
                CoUninitialize();
            }
        }
    }
}

/// Hardware H.264 encoder MFTs for NV12 input, best first (as Windows sorts them).
fn hardware_mfts() -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let mut out = Vec::new();
    unsafe {
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut arr, &mut n).is_err()
            || arr.is_null()
        {
            return out;
        }
        for slot in std::slice::from_raw_parts_mut(arr, n as usize) {
            if let Some(a) = slot.take() {
                out.push(a);
            }
        }
        CoTaskMemFree(Some(arr as *const _));
    }
    out
}

unsafe fn attr_string(a: &IMFActivate, key: &GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if a.GetAllocatedString(key, &mut p, &mut len).is_err() {
            return String::new();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Short name by GPU vendor (PCI vendor id in `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute`).
fn vendor_name(a: &IMFActivate) -> &'static str {
    let vendor = unsafe { attr_string(a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute) }.to_ascii_uppercase();
    let name = unsafe { attr_string(a, &MFT_FRIENDLY_NAME_Attribute) }.to_ascii_uppercase();
    if vendor.contains("10DE") || name.contains("NVIDIA") {
        "nvenc"
    } else if vendor.contains("1002") || vendor.contains("1022") || name.contains("AMD") {
        "amf"
    } else if vendor.contains("8086") || name.contains("INTEL") {
        "quicksync"
    } else {
        "mf-hardware"
    }
}

pub(crate) fn candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(_mf) = MfInit::new() else { return out };
    for (i, a) in hardware_mfts().iter().enumerate() {
        let name = vendor_name(a);
        out.push(Candidate {
            name,
            open: Box::new(move |cfg, w, h| {
                MfEncoder::open(Which::Hardware(i), name, cfg, w, h).map(|e| Box::new(e) as Box<dyn Backend>)
            }),
        });
    }
    out.push(Candidate {
        name: "mf-software",
        open: Box::new(|cfg, w, h| {
            MfEncoder::open(Which::Software, "mf-software", cfg, w, h).map(|e| Box::new(e) as Box<dyn Backend>)
        }),
    });
    out
}

#[derive(Clone, Copy)]
enum Which {
    /// Index into [`hardware_mfts`].
    Hardware(usize),
    Software,
}

fn var_u32(v: u32) -> VARIANT {
    let mut x = VARIANT::default();
    unsafe {
        let inner = &mut *x.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    x
}

fn var_bool(v: bool) -> VARIANT {
    let mut x = VARIANT::default();
    unsafe {
        let inner = &mut *x.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v { VARIANT_TRUE } else { Default::default() };
    }
    x
}

/// Maps a Windows error to a message naming the step that failed.
fn ctx(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {}", e.message())
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// One Media Foundation encoder instance for one frame size.
struct MfEncoder {
    name: &'static str,
    // Field order matters for drop: the transform goes before MF is shut down.
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    activate: Option<IMFActivate>,
    /// We must allocate output samples (the software MFT); `cbSize` of one.
    alloc_output: Option<u32>,
    /// Input requests announced by an async MFT and not yet served.
    need_input: u32,
    params: ParamSets,
    width: u32,
    height: u32,
    fps: f32,
    nv12: Vec<u8>,
    _mf: MfInit,
}

// The MFT is created, used and dropped on the encoder thread only (COM multithreaded
// apartment); moving the handle between threads before first use is fine.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    fn open(which: Which, name: &'static str, cfg: &EncoderConfig, w: u32, h: u32) -> Result<MfEncoder, String> {
        let mf = MfInit::new()?;
        unsafe {
            let (transform, activate) = match which {
                Which::Hardware(i) => {
                    let a = hardware_mfts().into_iter().nth(i).ok_or("hardware encoder is gone")?;
                    (a.ActivateObject::<IMFTransform>().map_err(ctx("activate"))?, Some(a))
                }
                Which::Software => (
                    CoCreateInstance::<_, IMFTransform>(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                        .map_err(ctx("create"))?,
                    None,
                ),
            };
            let attrs = transform.GetAttributes().ok();
            let is_async = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
            if is_async {
                attrs
                    .as_ref()
                    .ok_or("async encoder without attributes")?
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(ctx("unlock"))?;
            }
            if let Some(a) = &attrs {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let codec = transform.cast::<ICodecAPI>().ok();
            let mut enc = MfEncoder {
                name,
                events: if is_async { Some(transform.cast().map_err(ctx("events"))?) } else { None },
                transform,
                codec,
                activate,
                alloc_output: None,
                need_input: 0,
                params: ParamSets::default(),
                width: w,
                height: h,
                fps: cfg.max_fps.clamp(1.0, 60.0),
                nv12: Vec::new(),
                _mf: mf,
            };
            enc.configure(cfg, which)?;
            Ok(enc)
        }
    }

    fn set(&self, key: &GUID, v: VARIANT) -> bool {
        self.codec.as_ref().is_some_and(|c| unsafe { c.SetValue(key, &v) }.is_ok())
    }

    unsafe fn configure(&mut self, cfg: &EncoderConfig, which: Which) -> Result<(), String> {
        let (w, h) = (self.width, self.height);
        let fps = (self.fps.round() as u32).max(1);
        // Codec settings first: some encoders only take rate control before the media types.
        self.set(&CODECAPI_AVLowLatencyMode, var_bool(true));
        self.set(&CODECAPI_AVEncCommonRateControlMode, var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
        self.set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(cfg.bitrate_bps));
        self.set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0));
        // We place keyframes ourselves; the encoder's own GOP is only a backstop.
        let gop = (cfg.keyframe_interval.as_secs_f32() * fps as f32).clamp(fps as f32, 100_000.0) as u32;
        self.set(&CODECAPI_AVEncMPVGOPSize, var_u32(gop));
        if matches!(which, Which::Software) {
            // 0 = fastest, 100 = best. The CPU encoder must keep up at high pixel rates.
            let rate = w as u64 * h as u64 * fps as u64;
            let q = if rate > 1920 * 1080 * 30 {
                0
            } else if rate > 1280 * 720 * 30 {
                33
            } else {
                66
            };
            self.set(&CODECAPI_AVEncCommonQualityVsSpeed, var_u32(q));
        }
        unsafe {
            // Output first (encoders derive the input types they accept from it). High profile
            // without B-frames; OpenH264 decodes CABAC and 8x8 transforms. Older profiles as
            // fallbacks for encoders that refuse High.
            let mut set_out = Err("no profile accepted".to_string());
            for profile in [eAVEncH264VProfile_High, eAVEncH264VProfile_Main, eAVEncH264VProfile_Base] {
                let t = MFCreateMediaType().map_err(ctx("media type"))?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(ctx("type"))?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(ctx("type"))?;
                t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps).map_err(ctx("type"))?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(ctx("type"))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(ctx("type"))?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(ctx("type"))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(ctx("type"))?;
                t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32).map_err(ctx("type"))?;
                match self.transform.SetOutputType(0, &t, 0) {
                    Ok(()) => {
                        set_out = Ok(());
                        break;
                    }
                    Err(err) => set_out = Err(format!("output type: {}", err.message())),
                }
            }
            set_out?;
            let t = MFCreateMediaType().map_err(ctx("media type"))?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(ctx("type"))?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(ctx("type"))?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(ctx("type"))?;
            t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(ctx("type"))?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(ctx("type"))?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(ctx("type"))?;
            t.SetUINT32(&MF_MT_DEFAULT_STRIDE, w).map_err(ctx("type"))?;
            self.transform.SetInputType(0, &t, 0).map_err(ctx("input type"))?;
            // Settings some encoders only accept once the types are known.
            self.set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(cfg.bitrate_bps));
            self.seed_params();
            let info = self.transform.GetOutputStreamInfo(0).map_err(ctx("stream info"))?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            if info.dwFlags & provides == 0 {
                self.alloc_output = Some(info.cbSize.max(w * h * 3 / 2).max(1 << 16));
            }
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(ctx("begin streaming"))?;
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(ctx("start of stream"))?;
        }
        Ok(())
    }

    /// Parameter sets from the output type's sequence header, so IDRs can carry them even if
    /// the encoder only emits them once.
    unsafe fn seed_params(&mut self) {
        unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else { return };
            let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
            let mut blob = vec![0u8; n as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).is_ok() {
                let mut scratch = Vec::new();
                self.params.normalize(split_annexb(&blob), &mut scratch);
            }
        }
    }

    /// The frame as an NV12 sample.
    unsafe fn input_sample(&mut self, frame: &I420Frame, ts_ms: u64) -> windows::core::Result<IMFSample> {
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let len = w * h * 3 / 2;
        self.nv12.resize(len, 0);
        let (y, uv) = self.nv12.split_at_mut(w * h);
        y.copy_from_slice(frame.y());
        for ((d, &u), &v) in uv.as_chunks_mut::<2>().0.iter_mut().zip(frame.u()).zip(frame.v()) {
            *d = [u, v];
        }
        unsafe {
            let buf = MFCreateMemoryBuffer(len as u32)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), p, len);
            buf.Unlock()?;
            buf.SetCurrentLength(len as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(ts_ms as i64 * 10_000)?;
            s.SetSampleDuration((10_000_000.0 / self.fps as f64) as i64)?;
            Ok(s)
        }
    }

    /// Takes one output sample. `Ok(false)`: the encoder needs more input first.
    unsafe fn pull_output(&mut self, out: &mut Vec<Au>) -> Result<bool, String> {
        unsafe {
            let ours = match self.alloc_output {
                Some(size) => {
                    let s = MFCreateSample().map_err(|e| e.message())?;
                    s.AddBuffer(&MFCreateMemoryBuffer(size).map_err(|e| e.message())?).map_err(|e| e.message())?;
                    Some(s)
                }
                None => None,
            };
            let mut db = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.transform.ProcessOutput(0, &mut db, &mut status);
            let sample = ManuallyDrop::take(&mut db[0].pSample);
            drop(ManuallyDrop::take(&mut db[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // New output type (e.g. after a keyframe with a different header): accept
                    // what the encoder proposes and go on.
                    if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                        self.transform.SetOutputType(0, &t, 0).map_err(|e| e.message())?;
                    }
                    self.seed_params();
                    return Ok(true);
                }
                Err(e) => return Err(format!("encoding failed: {}", e.message())),
            }
            let Some(sample) = sample else { return Ok(true) };
            let ts_ms = sample.GetSampleTime().map(|t| (t.max(0) / 10_000) as u64).unwrap_or(0);
            let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.message())?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| e.message())?;
            let data = std::slice::from_raw_parts(p, len as usize);
            let mut au = Au { data: Vec::with_capacity(len as usize + 64), keyframe: false, ts_ms };
            au.keyframe = self.params.normalize(split_annexb(data), &mut au.data);
            let _ = buf.Unlock();
            if !au.data.is_empty() {
                out.push(au);
            }
            Ok(true)
        }
    }

    /// Handles queued events of an async MFT. Returns how many outputs were taken.
    unsafe fn pump(&mut self, out: &mut Vec<Au>) -> Result<usize, String> {
        let mut got = 0;
        let Some(events) = self.events.clone() else { return Ok(0) };
        loop {
            let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(got),
                Err(e) => return Err(format!("encoder events: {}", e.message())),
            };
            let kind = unsafe { ev.GetType() }.unwrap_or(0);
            if let Ok(hr) = unsafe { ev.GetStatus() }
                && hr.is_err()
            {
                return Err(format!("encoder error: {}", hr.message()));
            }
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                let before = out.len();
                unsafe { self.pull_output(out)? };
                got += (out.len() > before) as usize;
            }
        }
    }

    unsafe fn encode_async(&mut self, frame: &I420Frame, ts_ms: u64, out: &mut Vec<Au>) -> Result<(), String> {
        unsafe {
            // Wait (briefly) until the encoder asks for input.
            let deadline = Instant::now() + Duration::from_millis(200);
            self.pump(out)?;
            while self.need_input == 0 {
                if Instant::now() > deadline {
                    return Err("the encoder stopped asking for frames".into());
                }
                std::thread::sleep(Duration::from_millis(1));
                self.pump(out)?;
            }
            let sample = self.input_sample(frame, ts_ms).map_err(|e| e.message())?;
            self.transform.ProcessInput(0, &sample, 0).map_err(|e| format!("encoding failed: {}", e.message()))?;
            self.need_input -= 1;
            // Low-latency hardware encoders finish a frame in a few ms; wait for it so the
            // packets go out now rather than one frame later. A slower one is collected on
            // the next call.
            let deadline = Instant::now() + Duration::from_millis(30);
            let before = out.len();
            loop {
                self.pump(out)?;
                if out.len() > before || Instant::now() > deadline {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_micros(300));
            }
        }
    }

    unsafe fn encode_sync(&mut self, frame: &I420Frame, ts_ms: u64, out: &mut Vec<Au>) -> Result<(), String> {
        unsafe {
            let sample = self.input_sample(frame, ts_ms).map_err(|e| e.message())?;
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    while self.pull_output(out)? {}
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| format!("encoding failed: {}", e.message()))?;
                }
                Err(e) => return Err(format!("encoding failed: {}", e.message())),
            }
            while self.pull_output(out)? {}
            Ok(())
        }
    }
}

impl Backend for MfEncoder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn encode(&mut self, frame: &I420Frame, ts_ms: u64, key: bool, out: &mut Vec<Au>) -> Result<(), String> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err("frame size changed".into());
        }
        if key {
            self.set(&CODECAPI_AVEncVideoForceKeyFrame, var_u32(1));
        }
        unsafe {
            if self.events.is_some() {
                self.encode_async(frame, ts_ms, out)
            } else {
                self.encode_sync(frame, ts_ms, out)
            }
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(bps));
    }

    fn set_max_fps(&mut self, fps: f32) {
        // The media type's frame rate is fixed once streaming; sample durations carry it.
        self.fps = fps.clamp(1.0, 60.0);
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // Async MFTs keep an event queue (and threads) alive until shut down.
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            if let Some(a) = &self.activate {
                let _ = a.ShutdownObject();
            }
        }
    }
}

/// Encoder output has to be decodable by OpenH264 (what every receiver uses): run the real
/// encoders on GitHub's Windows runners (`cargo test --release -p celerity-stream`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwenc::self_test;

    fn config() -> EncoderConfig {
        EncoderConfig {
            content: crate::frame::ContentKind::Screen,
            bitrate_bps: 4_000_000,
            max_fps: 30.0,
            ..EncoderConfig::screen()
        }
    }

    #[test]
    fn software_mft_output_decodes_with_openh264() {
        let mut enc = match MfEncoder::open(Which::Software, "mf-software", &config(), 1280, 720) {
            Ok(e) => e,
            // Windows Server without the Media Foundation feature.
            Err(e) if e.contains("not available") => {
                eprintln!("skipped: {e}");
                return;
            }
            Err(e) => panic!("software H.264 MFT: {e}"),
        };
        self_test(&mut enc, 1280, 720, 12, 1.0).expect("self-test");
        // Forced keyframes and bitrate changes mid-stream.
        let mut out = Vec::new();
        let mut f = I420Frame::new(1280, 720);
        let mut keys = Vec::new();
        for n in 0..20u64 {
            crate::test_pattern::render_test_pattern(n, &mut f);
            if n == 10 {
                enc.set_bitrate(1_000_000);
            }
            enc.encode(&f, 1000 + n * 33, n == 0 || n == 15, &mut out).unwrap();
            for au in out.drain(..) {
                if au.keyframe {
                    keys.push(au.ts_ms);
                }
                // Every keyframe carries its parameter sets.
                if au.keyframe {
                    let units = split_annexb(&au.data);
                    assert!(units.iter().any(|u| u[0] & 0x1f == 7) && units.iter().any(|u| u[0] & 0x1f == 8));
                }
            }
        }
        assert!(keys.contains(&(1000 + 15 * 33)), "forced keyframe missing: {keys:?}");
    }

    #[test]
    fn hardware_mfts_pass_or_are_skipped() {
        // GitHub's runners have no GPU encoder; on a real machine this checks the async path.
        for c in candidates() {
            match (c.open)(&config(), 1920, 1080).and_then(|mut b| self_test(b.as_mut(), 1920, 1080, 8, 30.0)) {
                Ok(()) => eprintln!("{}: ok", c.name),
                Err(e) => eprintln!("{}: not usable: {e}", c.name),
            }
        }
    }

    #[test]
    fn sender_picks_an_mf_encoder() {
        let available = MfEncoder::open(Which::Software, "mf-software", &config(), 1280, 720).is_ok();
        // A low frame rate so a slow CI machine still passes the speed check of the self-test.
        let mut enc = crate::hwenc::VideoEncoder::new(EncoderConfig { max_fps: 10.0, ..config() }, true).unwrap();
        let mut dec = crate::h264::Decoder::new().unwrap();
        let mut f = I420Frame::new(1280, 720);
        let mut pictures = 0;
        for n in 0..10u64 {
            crate::test_pattern::render_test_pattern(n, &mut f);
            for au in enc.encode(&f, n * 33).unwrap() {
                pictures += dec.decode(&au.data).expect("decodable").is_some() as u32;
            }
        }
        eprintln!("encoder in use: {} ({pictures} pictures)", enc.name());
        assert!(pictures >= 8);
        if available {
            assert_ne!(enc.name(), "openh264", "the Media Foundation encoder was not used");
        }
    }
}
