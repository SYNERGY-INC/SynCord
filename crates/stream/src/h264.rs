//! Thin safe wrappers around the OpenH264 encoder and decoder (compiled from source by
//! `openh264-sys2`, the same build syncord-video uses).

use std::ffi::c_void;
use std::ptr::null_mut;
use std::time::Duration;

use openh264_sys2 as sys;
use sys::API;

use crate::convert::{self, YuvView};
use crate::frame::{ContentKind, I420Frame};

// ---------------------------------------------------------------------------------------------
// Encoder

/// Encoder settings. All of them except the content kind can be changed while running.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub content: ContentKind,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Upper bound on the frame rate; the rate controller budgets bits per frame from it.
    pub max_fps: f32,
    /// A keyframe is inserted when this much time passed since the last one (measured on frame
    /// timestamps). Keyframes also happen on [`Encoder::force_keyframe`] and size changes.
    pub keyframe_interval: Duration,
    /// OpenH264's screen-content mode (`SCREEN_CONTENT_REAL_TIME`) instead of its camera mode.
    /// Off by default, also for screens: on text it gave ~10% lower luma error at the same
    /// bitrate but cost 2-5x the encode CPU and ~1.5x the memory (see the `text_quality` test
    /// and the `bench` example). Fixed at creation.
    pub screen_content_tools: bool,
}

impl EncoderConfig {
    /// Defaults for a facecam: 700 kbps, 30 fps, keyframe every 4 s.
    pub fn camera() -> Self {
        EncoderConfig {
            content: ContentKind::Camera,
            bitrate_bps: 700_000,
            max_fps: 30.0,
            keyframe_interval: Duration::from_secs(4),
            screen_content_tools: false,
        }
    }

    /// Defaults for screen sharing: 2.5 Mbps, 15 fps, keyframe every 10 s (screen keyframes are
    /// large, and lost frames are repaired with on-demand keyframes anyway).
    pub fn screen() -> Self {
        EncoderConfig {
            content: ContentKind::Screen,
            bitrate_bps: 2_500_000,
            max_fps: 15.0,
            keyframe_interval: Duration::from_secs(10),
            screen_content_tools: false,
        }
    }
}

/// One encoded access unit (Annex B, with start codes; SPS/PPS precede every keyframe).
#[derive(Debug)]
pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    /// An IDR picture: decodable on its own.
    pub keyframe: bool,
}

/// H.264 Constrained Baseline encoder (no B-frames: every frame can be shown as soon as it is
/// decoded). The underlying encoder is created on the first frame and re-created when the frame
/// size changes.
pub struct Encoder {
    api: sys::DynamicAPI,
    ptr: *mut sys::ISVCEncoder,
    initialized: bool,
    size: (u32, u32),
    config: EncoderConfig,
    info: Box<sys::SFrameBSInfo>,
    out: Vec<u8>,
    force_key: bool,
    last_key_ms: Option<u64>,
}

// Plain heap object without thread affinity, only used through `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Encoder, String> {
        let api = sys::DynamicAPI::from_source();
        let mut ptr: *mut sys::ISVCEncoder = null_mut();
        unsafe {
            if api.WelsCreateSVCEncoder(&mut ptr) != 0 || ptr.is_null() {
                return Err("failed to create H.264 encoder".into());
            }
        }
        Ok(Encoder {
            api,
            ptr,
            initialized: false,
            size: (0, 0),
            config,
            info: Box::default(),
            out: Vec::new(),
            force_key: false,
            last_key_ms: None,
        })
    }

    fn uninit(&mut self) {
        if self.initialized {
            unsafe {
                let vt = &**self.ptr;
                (vt.Uninitialize.unwrap())(self.ptr);
            }
            self.initialized = false;
        }
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// The next encoded frame will be a keyframe.
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Changes the target bitrate (takes effect within a few frames).
    pub fn set_bitrate(&mut self, bps: u32) {
        self.config.bitrate_bps = bps.clamp(50_000, 50_000_000);
        if self.initialized {
            let mut max = sys::SBitrateInfo { iLayer: sys::SPATIAL_LAYER_ALL, iBitrate: self.max_bitrate() };
            let mut br = sys::SBitrateInfo { iLayer: sys::SPATIAL_LAYER_ALL, iBitrate: self.config.bitrate_bps as i32 };
            unsafe {
                // Raise the cap first so the target is never above it.
                self.set_option(sys::ENCODER_OPTION_MAX_BITRATE, (&raw mut max).cast());
                self.set_option(sys::ENCODER_OPTION_BITRATE, (&raw mut br).cast());
                self.set_option(sys::ENCODER_OPTION_MAX_BITRATE, (&raw mut max).cast());
            }
        }
    }

    /// Changes the frame rate the rate controller budgets for.
    pub fn set_max_fps(&mut self, fps: f32) {
        self.config.max_fps = fps.clamp(1.0, 60.0);
        if self.initialized {
            let mut f = self.config.max_fps;
            unsafe { self.set_option(sys::ENCODER_OPTION_FRAME_RATE, (&raw mut f).cast()) };
        }
    }

    pub fn set_keyframe_interval(&mut self, d: Duration) {
        self.config.keyframe_interval = d;
    }

    fn max_bitrate(&self) -> i32 {
        // Allow short bursts above the target (keyframes, scene changes), as WebRTC does.
        (self.config.bitrate_bps as u64 * 3 / 2).min(i32::MAX as u64) as i32
    }

    unsafe fn set_option(&mut self, opt: sys::ENCODER_OPTION, val: *mut c_void) -> i32 {
        unsafe {
            let vt = &**self.ptr;
            (vt.SetOption.unwrap())(self.ptr, opt, val)
        }
    }

    fn init(&mut self, w: u32, h: u32) -> Result<(), String> {
        // OpenH264 supports up to level 5.2.
        if w.max(h) > 3840 || w.min(h) > 2160 {
            return Err(format!("frame too large for H.264 encoding: {w}x{h}"));
        }
        self.uninit();
        let screen = self.config.content == ContentKind::Screen;
        let screen_usage = self.config.screen_content_tools;
        let mut p = sys::SEncParamExt::default();
        unsafe {
            let vt = &**self.ptr;
            (vt.GetDefaultParams.unwrap())(self.ptr, &mut p);
        }
        p.iUsageType = if screen_usage { sys::SCREEN_CONTENT_REAL_TIME } else { sys::CAMERA_VIDEO_REAL_TIME };
        p.iPicWidth = w as i32;
        p.iPicHeight = h as i32;
        p.iTargetBitrate = self.config.bitrate_bps as i32;
        p.iMaxBitrate = self.max_bitrate();
        p.iRCMode = sys::RC_BITRATE_MODE;
        p.fMaxFrameRate = self.config.max_fps;
        p.iTemporalLayerNum = 1;
        p.iSpatialLayerNum = 1;
        // We decide on keyframes ourselves (time based, or on request).
        p.uiIntraPeriod = 0;
        p.iNumRefFrame = 1;
        // Same SPS/PPS ids every time: each IDR carries its parameter sets and stands alone.
        p.eSpsPpsIdStrategy = sys::CONSTANT_ID;
        p.bPrefixNalAddingCtrl = false;
        p.bEnableSSEI = false;
        p.iPaddingFlag = 0;
        p.iEntropyCodingModeFlag = 0; // CAVLC (Constrained Baseline)
        // Skipping frames keeps the bitrate honest during big changes (e.g. scrolling a page).
        p.bEnableFrameSkip = true;
        p.iMinQp = if screen { 12 } else { 16 };
        p.iMaxQp = if screen { 40 } else { 42 };
        p.bEnableLongTermReference = false;
        // One thread per stream keeps CPU use predictable; big screen frames get two.
        p.iMultipleThreadIdc = if w * h > 1280 * 720 { 2 } else { 1 };
        p.iLoopFilterDisableIdc = 0;
        p.bEnableDenoise = false;
        p.bEnableBackgroundDetection = true;
        p.bEnableAdaptiveQuant = !screen;
        p.bEnableFrameCroppingFlag = true;
        p.bEnableSceneChangeDetect = true;
        p.iComplexityMode = sys::LOW_COMPLEXITY;
        let l = &mut p.sSpatialLayers[0];
        l.iVideoWidth = w as i32;
        l.iVideoHeight = h as i32;
        l.fFrameRate = self.config.max_fps;
        l.iSpatialBitrate = self.config.bitrate_bps as i32;
        l.iMaxSpatialBitrate = self.max_bitrate();
        l.uiProfileIdc = sys::PRO_BASELINE;
        l.sSliceArgument.uiSliceMode =
            if p.iMultipleThreadIdc > 1 { sys::SM_FIXEDSLCNUM_SLICE } else { sys::SM_SINGLE_SLICE };
        l.sSliceArgument.uiSliceNum = p.iMultipleThreadIdc as u32;
        // Signal BT.709 limited range, which is what our converters produce and assume.
        l.bVideoSignalTypePresent = true;
        l.uiVideoFormat = 5; // unspecified
        l.bFullRange = false;
        l.bColorDescriptionPresent = true;
        l.uiColorPrimaries = 1;
        l.uiTransferCharacteristics = 1;
        l.uiColorMatrix = 1;
        unsafe {
            let vt = &**self.ptr;
            let mut level: i32 = sys::WELS_LOG_QUIET;
            (vt.SetOption.unwrap())(self.ptr, sys::ENCODER_OPTION_TRACE_LEVEL, (&raw mut level).cast());
            let rc = (vt.InitializeExt.unwrap())(self.ptr, &p);
            if rc != 0 {
                return Err(format!("failed to initialize H.264 encoder ({w}x{h}, error {rc})"));
            }
            let mut fmt: i32 = sys::videoFormatI420;
            (vt.SetOption.unwrap())(self.ptr, sys::ENCODER_OPTION_DATAFORMAT, (&raw mut fmt).cast());
        }
        self.initialized = true;
        self.size = (w, h);
        self.last_key_ms = None;
        Ok(())
    }

    /// Encodes one frame. `timestamp_ms` must not go backwards. Returns `Ok(None)` when the rate
    /// controller skipped the frame to stay within the bitrate.
    pub fn encode(&mut self, frame: &I420Frame, timestamp_ms: u64) -> Result<Option<EncodedFrame<'_>>, String> {
        let (w, h) = (frame.width(), frame.height());
        if !self.initialized || self.size != (w, h) {
            self.init(w, h)?;
        }
        let due = self
            .last_key_ms
            .is_none_or(|k| timestamp_ms.saturating_sub(k) >= self.config.keyframe_interval.as_millis().max(1) as u64);
        if self.force_key || (due && self.last_key_ms.is_some()) {
            unsafe {
                let vt = &**self.ptr;
                (vt.ForceIntraFrame.unwrap())(self.ptr, true);
            }
        }
        self.force_key = false;
        let pic = sys::SSourcePicture {
            iColorFormat: sys::videoFormatI420,
            iStride: [w as i32, (w / 2) as i32, (w / 2) as i32, 0],
            pData: [
                frame.y().as_ptr().cast_mut(),
                frame.u().as_ptr().cast_mut(),
                frame.v().as_ptr().cast_mut(),
                null_mut(),
            ],
            iPicWidth: w as i32,
            iPicHeight: h as i32,
            uiTimeStamp: timestamp_ms as i64,
            ..Default::default()
        };
        let rc = unsafe {
            let vt = &**self.ptr;
            (vt.EncodeFrame.unwrap())(self.ptr, &pic, &mut *self.info)
        };
        if rc != 0 {
            // Re-create on the next frame; a failed encoder state is not trustworthy.
            self.uninit();
            return Err(format!("H.264 encoding failed (error {rc})"));
        }
        let info = &*self.info;
        if info.eFrameType == sys::videoFrameTypeSkip || info.eFrameType == sys::videoFrameTypeInvalid {
            return Ok(None);
        }
        self.out.clear();
        for layer in &info.sLayerInfo[..info.iLayerNum.clamp(0, 128) as usize] {
            let mut off = 0usize;
            for n in 0..layer.iNalCount.max(0) as usize {
                let len = unsafe { *layer.pNalLengthInByte.add(n) }.max(0) as usize;
                let nal = unsafe { std::slice::from_raw_parts(layer.pBsBuf.add(off), len) };
                self.out.extend_from_slice(nal);
                off += len;
            }
        }
        if self.out.is_empty() {
            return Ok(None);
        }
        let keyframe = info.eFrameType == sys::videoFrameTypeIDR;
        if keyframe {
            self.last_key_ms = Some(timestamp_ms);
        }
        Ok(Some(EncodedFrame { data: &self.out, keyframe }))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                let vt = &**self.ptr;
                (vt.Uninitialize.unwrap())(self.ptr);
            }
            self.api.WelsDestroySVCEncoder(self.ptr);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder

/// A decoded picture borrowed from the decoder (valid until the next decode call).
pub struct DecodedPicture<'a> {
    view: YuvView<'a>,
}

impl DecodedPicture<'_> {
    pub fn width(&self) -> u32 {
        self.view.width as u32
    }

    pub fn height(&self) -> u32 {
        self.view.height as u32
    }

    /// Converts to RGBA8 with the longest side at most `max_side` (never upscaled), into `out`.
    /// Returns the output size.
    pub fn to_rgba(&self, max_side: u32, out: &mut Vec<u8>) -> (u32, u32) {
        let (w, h) = convert::fit_side(self.width(), self.height(), max_side);
        convert::to_rgba(&self.view, w, h, out);
        (w, h)
    }

    /// Copies the picture into an [`I420Frame`] (cropped to even size).
    pub fn to_i420(&self, out: &mut I420Frame) {
        let v = &self.view;
        let raw = convert::RawFrame::I420 {
            y: v.y,
            u: v.u,
            v: v.v,
            y_stride: v.stride_y,
            uv_stride: v.stride_uv,
            width: v.width,
            height: v.height,
        };
        let mut c = convert::Converter::default();
        c.convert_to(&raw, v.width as u32 & !1, v.height as u32 & !1, out);
    }
}

/// Why a decode call did not produce a clean picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The data was corrupt or referenced a frame the decoder never got. Ask for a keyframe.
    Corrupt,
}

/// H.264 decoder for our own streams (and any Baseline/Main/High 8-bit 4:2:0 stream).
pub struct Decoder {
    api: sys::DynamicAPI,
    ptr: *mut sys::ISVCDecoder,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        let api = sys::DynamicAPI::from_source();
        let mut ptr: *mut sys::ISVCDecoder = null_mut();
        unsafe {
            if api.WelsCreateDecoder(&mut ptr) != 0 || ptr.is_null() {
                return Err("failed to create H.264 decoder".into());
            }
            let dec = Decoder { api, ptr };
            let vt = &**dec.ptr;
            let mut level: i32 = sys::WELS_LOG_QUIET;
            (vt.SetOption.unwrap())(dec.ptr, sys::DECODER_OPTION_TRACE_LEVEL, (&raw mut level).cast::<c_void>());
            let param = sys::SDecodingParam {
                // Corrupt frames are reported (and we ask for a keyframe) rather than concealed
                // into smeared garbage that would persist until the next keyframe anyway.
                eEcActiveIdc: sys::ERROR_CON_DISABLE,
                sVideoProperty: sys::SVideoProperty {
                    size: std::mem::size_of::<sys::SVideoProperty>() as u32,
                    eVideoBsType: sys::VIDEO_BITSTREAM_AVC,
                },
                ..Default::default()
            };
            if (vt.Initialize.unwrap())(dec.ptr, &param) != 0 {
                return Err("failed to initialize H.264 decoder".into());
            }
            (vt.SetOption.unwrap())(dec.ptr, sys::DECODER_OPTION_TRACE_LEVEL, (&raw mut level).cast::<c_void>());
            Ok(dec)
        }
    }

    /// Decodes one access unit (Annex B). `Ok(None)` means the data was fine but produced no
    /// picture (e.g. parameter sets only).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedPicture<'_>>, DecodeError> {
        let mut dst = [null_mut::<u8>(); 3];
        let mut info = sys::SBufferInfo::default();
        let state = unsafe {
            let vt = &**self.ptr;
            (vt.DecodeFrameNoDelay.unwrap())(self.ptr, data.as_ptr(), data.len() as i32, dst.as_mut_ptr(), &mut info)
        };
        // dsErrorFree = 0; dsFramePending / dsNoParamSets etc. are flags.
        if state != 0 {
            return Err(DecodeError::Corrupt);
        }
        if info.iBufferStatus != 1 || dst.iter().any(|p| p.is_null()) {
            return Ok(None);
        }
        let sb = unsafe { info.UsrData.sSystemBuffer };
        let (w, h) = (sb.iWidth.max(0) as usize, sb.iHeight.max(0) as usize);
        let (sy, suv) = (sb.iStride[0].max(0) as usize, sb.iStride[1].max(0) as usize);
        if w < 2 || h < 2 || sy < w || suv < w.div_ceil(2) {
            return Ok(None);
        }
        let ch = h.div_ceil(2);
        let view = unsafe {
            YuvView {
                y: std::slice::from_raw_parts(dst[0], sy * (h - 1) + w),
                u: std::slice::from_raw_parts(dst[1], suv * (ch - 1) + w.div_ceil(2)),
                v: std::slice::from_raw_parts(dst[2], suv * (ch - 1) + w.div_ceil(2)),
                width: w,
                height: h,
                stride_y: sy,
                stride_uv: suv,
            }
        };
        Ok(Some(DecodedPicture { view }))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let vt = &**self.ptr;
            (vt.Uninitialize.unwrap())(self.ptr);
            self.api.WelsDestroyDecoder(self.ptr);
        }
    }
}
