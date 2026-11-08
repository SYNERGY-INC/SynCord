//! H.264 helpers: SPS parsing (for size / support checks) and a thin safe wrapper around the
//! OpenH264 decoder compiled from source by `openh264-sys2`.

use std::ffi::c_void;
use std::ptr::null_mut;

use openh264_sys2 as sys;
use sys::API;

// ---------------------------------------------------------------------------------------------
// Bit reader

/// MSB-first bit reader with Exp-Golomb support. Emulation prevention bytes must already be
/// removed (see [`unescape`]).
pub(crate) struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    pub fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }

    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Removes emulation prevention bytes (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// SPS

/// YUV -> RGB matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Debug)]
pub(crate) struct SpsInfo {
    pub profile: u8,
    pub chroma_format: u32,
    pub bit_depth: u32,
    pub frame_mbs_only: bool,
    /// Decoded picture size after cropping.
    pub width: u32,
    pub height: u32,
    /// Sample aspect ratio (1:1 when unknown).
    pub sar: (u32, u32),
    pub full_range: bool,
    pub matrix: Matrix,
}

impl SpsInfo {
    /// Display size, applying the sample aspect ratio to the width.
    pub fn display_size(&self) -> (u32, u32) {
        let (n, d) = self.sar;
        if n == 0 || d == 0 || n == d {
            return (self.width, self.height);
        }
        let w = (self.width as u64 * n as u64 / d as u64).clamp(1, 16384) as u32;
        (w, self.height)
    }

    /// Rejects streams OpenH264 can't decode, with a readable reason.
    pub fn check_supported(&self) -> Result<(), String> {
        match self.profile {
            66 | 77 | 100 => {}
            88 => return Err("H.264 Extended profile is not supported".into()),
            110 => return Err("10-bit H.264 (High 10 profile) is not supported".into()),
            122 => return Err("4:2:2 H.264 (High 4:2:2 profile) is not supported".into()),
            244 | 44 => return Err("4:4:4 H.264 (High 4:4:4 profile) is not supported".into()),
            p => return Err(format!("H.264 profile {p} is not supported")),
        }
        if self.chroma_format != 1 {
            return Err("only 4:2:0 H.264 is supported".into());
        }
        if self.bit_depth != 8 {
            return Err("only 8-bit H.264 is supported".into());
        }
        if !self.frame_mbs_only {
            return Err("interlaced H.264 is not supported".into());
        }
        if self.width == 0 || self.height == 0 || self.width > 8192 || self.height > 8192 {
            return Err(format!("unsupported video size {}x{}", self.width, self.height));
        }
        Ok(())
    }
}

pub(crate) fn parse_sps(nal: &[u8]) -> Result<SpsInfo, String> {
    parse_sps_inner(&unescape(nal)).ok_or_else(|| "invalid H.264 sequence parameter set".to_string())
}

fn parse_sps_inner(d: &[u8]) -> Option<SpsInfo> {
    let mut b = Bits::new(d);
    let nal_type = b.u(8)? & 0x1f;
    if nal_type != 7 {
        return None;
    }
    let profile = b.u(8)? as u8;
    b.u(16)?; // constraint flags + level
    b.ue()?; // sps id
    let mut chroma_format = 1;
    let mut bit_depth = 8;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format = b.ue()?;
        if chroma_format == 3 {
            b.bit()?; // separate_colour_plane
        }
        bit_depth = b.ue()? + 8;
        let _chroma_depth = b.ue()?;
        b.bit()?; // qpprime_y_zero_transform_bypass
        if b.bit()? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_allowed
    let w_mbs = b.ue()? as u64 + 1;
    let h_units = b.ue()? as u64 + 1;
    let frame_mbs_only = b.bit()? == 1;
    if !frame_mbs_only {
        b.bit()?; // mb_adaptive_frame_field
    }
    b.bit()?; // direct_8x8_inference
    let mut width = w_mbs * 16;
    let mut height = h_units * 16 * if frame_mbs_only { 1 } else { 2 };
    if b.bit()? == 1 {
        let (cx, cy) = match chroma_format {
            0 => (1, 1),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let cy = cy * if frame_mbs_only { 1 } else { 2 };
        let (l, r, t, bo) = (b.ue()? as u64, b.ue()? as u64, b.ue()? as u64, b.ue()? as u64);
        width = width.saturating_sub((l + r) * cx);
        height = height.saturating_sub((t + bo) * cy);
    }
    let mut sar = (1, 1);
    let mut full_range = false;
    let mut matrix = None;
    // VUI: only the leading fields we care about; a truncated VUI just keeps the defaults.
    if b.bit() == Some(1) {
        let mut vui = || -> Option<()> {
            if b.bit()? == 1 {
                let idc = b.u(8)?;
                const SARS: [(u32, u32); 17] = [
                    (0, 0),
                    (1, 1),
                    (12, 11),
                    (10, 11),
                    (16, 11),
                    (40, 33),
                    (24, 11),
                    (20, 11),
                    (32, 11),
                    (80, 33),
                    (18, 11),
                    (15, 11),
                    (64, 33),
                    (160, 99),
                    (4, 3),
                    (3, 2),
                    (2, 1),
                ];
                if idc == 255 {
                    sar = (b.u(16)?, b.u(16)?);
                } else if let Some(&s) = SARS.get(idc as usize) {
                    sar = s;
                }
            }
            if b.bit()? == 1 {
                b.bit()?; // overscan_appropriate
            }
            if b.bit()? == 1 {
                b.u(3)?; // video_format
                full_range = b.bit()? == 1;
                if b.bit()? == 1 {
                    b.u(8)?; // primaries
                    b.u(8)?; // transfer
                    matrix = match b.u(8)? {
                        1 => Some(Matrix::Bt709),
                        5 | 6 => Some(Matrix::Bt601),
                        9 | 10 => Some(Matrix::Bt2020),
                        _ => None,
                    };
                }
            }
            Some(())
        };
        let _ = vui();
    }
    let (width, height) = (width.min(u32::MAX as u64) as u32, height.min(u32::MAX as u64) as u32);
    // Unspecified matrix: the usual convention is BT.601 for SD and BT.709 for HD.
    let matrix = matrix.unwrap_or(if height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 });
    Some(SpsInfo { profile, chroma_format, bit_depth, frame_mbs_only, width, height, sar, full_range, matrix })
}

// ---------------------------------------------------------------------------------------------
// Annex B conversion

/// Converts length-prefixed NAL units (as stored in MP4 samples) to Annex B start-code format,
/// which is what OpenH264 consumes. Appends to `out`.
pub(crate) fn append_annexb(sample: &[u8], nal_len: usize, out: &mut Vec<u8>) {
    let mut p = 0usize;
    while p + nal_len <= sample.len() {
        let mut n = 0usize;
        for &b in &sample[p..p + nal_len] {
            n = (n << 8) | b as usize;
        }
        p += nal_len;
        if n == 0 || p + n > sample.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[p..p + n]);
        p += n;
    }
}

/// `true` if the access unit (length-prefixed, as in MP4) is a non-reference picture
/// (`nal_ref_idc == 0`): no other picture is predicted from it, so it can be skipped without
/// affecting the frames after it.
pub(crate) fn is_nonref(sample: &[u8], nal_len: usize) -> bool {
    let mut p = 0usize;
    while p + nal_len < sample.len() {
        let n = sample[p..p + nal_len].iter().fold(0usize, |n, &b| (n << 8) | b as usize);
        p += nal_len;
        if n == 0 || p + n > sample.len() {
            return false;
        }
        let hdr = sample[p];
        match hdr & 0x1f {
            1 => return hdr & 0x60 == 0,
            5 => return false,
            _ => {}
        }
        p += n;
    }
    false
}

pub(crate) fn param_sets_annexb(sets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in sets {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(s);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Decoder

/// A decoded picture borrowed from the decoder (valid until the next decode call).
pub(crate) struct Picture<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride_y: usize,
    pub stride_uv: usize,
    /// The timestamp that was passed in with the access unit this picture came from.
    pub ts: u64,
}

pub(crate) struct Decoder {
    api: sys::DynamicAPI,
    ptr: *mut sys::ISVCDecoder,
}

// The decoder is a plain heap object without thread affinity; we only ever use it from one
// thread at a time (`&mut self`).
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, String> {
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
                eEcActiveIdc: sys::ERROR_CON_SLICE_MV_COPY_CROSS_IDR,
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

    /// Feeds one access unit (Annex B) tagged with `ts`. Returns a picture if one is ready
    /// (possibly from an earlier access unit; check `Picture::ts`). Decode errors are not fatal:
    /// corrupt frames are simply not returned.
    pub fn decode(&mut self, annexb: &[u8], ts: u64) -> Option<Picture<'_>> {
        let mut dst = [null_mut::<u8>(); 3];
        let mut info = sys::SBufferInfo { uiInBsTimeStamp: ts, ..Default::default() };
        unsafe {
            let vt = &**self.ptr;
            (vt.DecodeFrameNoDelay.unwrap())(
                self.ptr,
                annexb.as_ptr(),
                annexb.len() as i32,
                dst.as_mut_ptr(),
                &mut info,
            );
            Self::picture(&dst, &info)
        }
    }

    /// Returns buffered pictures (held for reordering) one by one after the last access unit.
    pub fn flush(&mut self) -> Option<Picture<'_>> {
        unsafe {
            let vt = &**self.ptr;
            let mut remaining: i32 = 0;
            (vt.GetOption.unwrap())(
                self.ptr,
                sys::DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER,
                (&raw mut remaining).cast::<c_void>(),
            );
            if remaining <= 0 {
                return None;
            }
            let mut dst = [null_mut::<u8>(); 3];
            let mut info = sys::SBufferInfo::default();
            (vt.FlushFrame.unwrap())(self.ptr, dst.as_mut_ptr(), &mut info);
            Self::picture(&dst, &info)
        }
    }

    unsafe fn picture<'a>(dst: &[*mut u8; 3], info: &sys::SBufferInfo) -> Option<Picture<'a>> {
        if info.iBufferStatus != 1 || dst.iter().any(|p| p.is_null()) {
            return None;
        }
        let sb = unsafe { info.UsrData.sSystemBuffer };
        let (w, h) = (sb.iWidth.max(0) as usize, sb.iHeight.max(0) as usize);
        let (sy, suv) = (sb.iStride[0].max(0) as usize, sb.iStride[1].max(0) as usize);
        if w == 0 || h == 0 || sy < w || suv < w.div_ceil(2) {
            return None;
        }
        let ch = h.div_ceil(2);
        unsafe {
            Some(Picture {
                y: std::slice::from_raw_parts(dst[0], sy * (h - 1) + w),
                u: std::slice::from_raw_parts(dst[1], suv * (ch - 1) + w.div_ceil(2)),
                v: std::slice::from_raw_parts(dst[2], suv * (ch - 1) + w.div_ceil(2)),
                width: w,
                height: h,
                stride_y: sy,
                stride_uv: suv,
                ts: info.uiOutYuvTimeStamp,
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 => ue 0, 1, 2, 3
        let d = [0b1010_0110, 0b0100_0000];
        let mut b = Bits::new(&d);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
    }

    #[test]
    fn annexb() {
        let mut out = Vec::new();
        append_annexb(&[0, 0, 0, 2, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc], 4, &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc]);
    }

    #[test]
    fn nonref_detection() {
        // SEI, then a non-reference slice (nal_ref_idc 0, type 1).
        assert!(is_nonref(&[0, 0, 0, 2, 0x06, 0xff, 0, 0, 0, 2, 0x01, 0x88], 4));
        // Reference P slice (nal_ref_idc 2), IDR, and garbage.
        assert!(!is_nonref(&[0, 0, 0, 2, 0x41, 0x88], 4));
        assert!(!is_nonref(&[0, 0, 0, 2, 0x65, 0x88], 4));
        assert!(!is_nonref(&[0, 0, 0, 9, 0x01], 4));
        assert!(!is_nonref(&[], 4));
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), [0, 0, 1, 0, 0]);
    }
}
