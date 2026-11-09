//! Pixel format conversion fused with area-averaging (box filter) downscaling, so capture output
//! goes straight from the device format to I420 at the target size without a full-size
//! intermediate copy, and decoded I420 goes straight to RGBA at the display size.
//!
//! Colour: I420 here is BT.709 limited range (what the encoder signals in its SPS).

use crate::frame::I420Frame;

/// A captured picture in the device's native layout, borrowed from the capture API.
#[derive(Clone, Copy)]
#[cfg_attr(any(not(target_os = "linux"), not(feature = "capture")), allow(dead_code))] // packed/JPEG: V4L2 only
pub(crate) enum RawFrame<'a> {
    /// 8-bit B, G, R, A/X per pixel (Windows DXGI/GDI, X11 ZPixmap depth 24/32).
    Bgra { data: &'a [u8], stride: usize, width: usize, height: usize },
    /// 8-bit R, G, B, A per pixel.
    Rgba { data: &'a [u8], stride: usize, width: usize, height: usize },
    /// Y plane + interleaved UV plane (most cameras, macOS capture, Media Foundation output).
    Nv12 { y: &'a [u8], y_stride: usize, uv: &'a [u8], uv_stride: usize, width: usize, height: usize },
    /// Planar Y, U, V 4:2:0.
    I420 { y: &'a [u8], u: &'a [u8], v: &'a [u8], y_stride: usize, uv_stride: usize, width: usize, height: usize },
    /// Packed Y0 U Y1 V (UVC webcams).
    Yuyv { data: &'a [u8], stride: usize, width: usize, height: usize },
    /// Packed U Y0 V Y1.
    Uyvy { data: &'a [u8], stride: usize, width: usize, height: usize },
    /// Interleaved full-range (JPEG) Y Cb Cr, 3 bytes per pixel (decoded MJPEG).
    YccFull { data: &'a [u8], width: usize, height: usize },
}

impl RawFrame<'_> {
    pub(crate) fn size(&self) -> (usize, usize) {
        match *self {
            RawFrame::Bgra { width, height, .. }
            | RawFrame::Rgba { width, height, .. }
            | RawFrame::Nv12 { width, height, .. }
            | RawFrame::I420 { width, height, .. }
            | RawFrame::Yuyv { width, height, .. }
            | RawFrame::Uyvy { width, height, .. }
            | RawFrame::YccFull { width, height, .. } => (width, height),
        }
    }
}

/// Output size for a `w`x`h` source so it fits in `max_w`x`max_h`: keeps the aspect ratio, never
/// upscales, rounds to even, at least 2x2.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as u64, h.max(1) as u64);
    let (mw, mh) = (max_w.max(2) as u64, max_h.max(2) as u64);
    let (ow, oh) = if w <= mw && h <= mh {
        (w, h)
    } else if w * mh >= h * mw {
        (mw, (h * mw + w / 2) / w)
    } else {
        ((w * mh + h / 2) / h, mh)
    };
    (((ow as u32) & !1).max(2), ((oh as u32) & !1).max(2))
}

/// Longest side limited to `max_side` (never upscales), even sizes.
pub(crate) fn fit_side(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    fit_size(w, h, max_side, max_side)
}

/// Reusable buffers so steady-state conversion allocates nothing.
#[derive(Default)]
pub(crate) struct Converter {
    /// Full-resolution I420 intermediate for RGB sources that also need scaling.
    full: I420Frame,
    taps: Vec<Taps>,
    acc: Vec<u32>,
}

/// One 8-bit sample plane: sample `x` of row `y` is `data[y * stride + x * step]`.
#[derive(Clone, Copy)]
struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
    step: usize,
    width: usize,
    height: usize,
}

impl<'a> Plane<'a> {
    fn of(data: &'a [u8], offset: usize, stride: usize, step: usize, width: usize, height: usize) -> Self {
        Plane { data: data.get(offset..).unwrap_or(&[]), stride, step, width, height }
    }

    /// Row `y`, or an empty slice if the buffer is short (bad stride from a driver).
    fn row(&self, y: usize) -> &'a [u8] {
        let start = y * self.stride;
        let need = (self.width - 1) * self.step + 1;
        self.data.get(start..start + need).unwrap_or(&[])
    }
}

const FULL_TO_LIMITED_Y: [u8; 256] = range_lut(219);
const FULL_TO_LIMITED_C: [u8; 256] = range_lut(224);

const fn range_lut(scale: u32) -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if scale == 219 {
            (16 + (i as u32 * 219 + 127) / 255) as u8
        } else {
            // Chroma is centred on 128; round half away from zero so the mapping is symmetric.
            let d = (i as i32 - 128) * 224;
            (128 + if d >= 0 { (d + 127) / 255 } else { (d - 127) / 255 }) as u8
        };
        i += 1;
    }
    t
}

/// Fixed-point weights sum to this.
const ONE: u32 = 1 << 14;

/// Area-averaging (box) resampling taps along one axis: output pixel `i` is the weighted sum of
/// source pixels `start[i] .. start[i] + k` with `w[i * k ..][..k]` (summing to [`ONE`]). Unlike
/// whole-pixel spans, fractional coverage keeps text strokes even at 1.5x and 1.33x.
struct Taps {
    src: usize,
    dst: usize,
    k: usize,
    start: Vec<u32>,
    w: Vec<u16>,
}

impl Taps {
    fn new(src: usize, dst: usize) -> Taps {
        let scale = src as f64 / dst as f64;
        let k = ((scale.ceil() as usize) + 1).clamp(1, src.max(1));
        let mut start = Vec::with_capacity(dst);
        let mut w = Vec::with_capacity(dst * k);
        for i in 0..dst {
            let (a, b) = (i as f64 * scale, ((i + 1) as f64 * scale).min(src as f64));
            let s = (a.floor() as usize).min(src - k);
            start.push(s as u32);
            let mut ws = vec![0u32; k];
            let mut sum = 0;
            for (j, wj) in ws.iter_mut().enumerate().take(k) {
                let (p0, p1) = ((s + j) as f64, (s + j + 1) as f64);
                let overlap = (p1.min(b) - p0.max(a)).max(0.0);
                *wj = (overlap / (b - a) * ONE as f64).round() as u32;
                sum += *wj;
            }
            // Put the rounding error on the heaviest tap so the weights sum to exactly ONE.
            let (big, _) = ws[..k].iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            ws[big] = (ws[big] + ONE).saturating_sub(sum).min(ONE);
            w.extend(ws[..k].iter().map(|&v| v as u16));
        }
        Taps { src, dst, k, start, w }
    }
}

impl Converter {
    /// Converts `raw` to I420 into `out`, scaled to fit `max_w`x`max_h` (never upscaled).
    pub(crate) fn convert(&mut self, raw: &RawFrame<'_>, max_w: u32, max_h: u32, out: &mut I420Frame) {
        let (sw, sh) = raw.size();
        let (dw, dh) = fit_size(sw as u32, sh as u32, max_w, max_h);
        self.convert_to(raw, dw, dh, out);
    }

    /// Converts `raw` to I420 at exactly `dw`x`dh` (rounded down to even; aspect not preserved).
    pub(crate) fn convert_to(&mut self, raw: &RawFrame<'_>, dw: u32, dh: u32, out: &mut I420Frame) {
        let (sw, sh) = raw.size();
        out.resize(dw, dh);
        let (dw, dh) = (out.width() as usize, out.height() as usize);
        if sw < 2 || sh < 2 {
            return;
        }
        // Odd sources are cropped by one pixel rather than resampled.
        let same = dw == sw & !1 && dh == sh & !1;
        let (cw, ch) = (dw / 2, dh / 2);
        let (scw, sch) = (sw.div_ceil(2), sh.div_ceil(2));
        match *raw {
            RawFrame::Bgra { data, stride, .. } | RawFrame::Rgba { data, stride, .. } => {
                let bgr = matches!(raw, RawFrame::Bgra { .. });
                if same {
                    rgb_to_i420_full(data, stride, bgr, out);
                } else {
                    let mut full = std::mem::take(&mut self.full);
                    full.resize(sw as u32, sh as u32);
                    rgb_to_i420_full(data, stride, bgr, &mut full);
                    self.i420(&YuvView::from(&full), out);
                    self.full = full;
                }
            }
            RawFrame::Nv12 { y, y_stride, uv, uv_stride, .. } => {
                let (yp, up, vp) = out.planes_mut();
                self.plane(Plane::of(y, 0, y_stride, 1, sw, sh), yp, dw, dh, None);
                if same {
                    for (cy, (ur, vr)) in up.chunks_exact_mut(cw).zip(vp.chunks_exact_mut(cw)).enumerate() {
                        let row = uv.get(cy * uv_stride..cy * uv_stride + cw * 2).unwrap_or(&[]);
                        for ((u, v), p) in ur.iter_mut().zip(vr.iter_mut()).zip(row.as_chunks::<2>().0.iter()) {
                            *u = p[0];
                            *v = p[1];
                        }
                    }
                } else {
                    self.plane(Plane::of(uv, 0, uv_stride, 2, scw, sch), up, cw, ch, None);
                    self.plane(Plane::of(uv, 1, uv_stride, 2, scw, sch), vp, cw, ch, None);
                }
            }
            RawFrame::I420 { y, u, v, y_stride, uv_stride, .. } => {
                let view = YuvView { y, u, v, width: sw, height: sh, stride_y: y_stride, stride_uv: uv_stride };
                self.i420(&view, out);
            }
            RawFrame::Yuyv { data, stride, .. } | RawFrame::Uyvy { data, stride, .. } => {
                // Y0 U Y1 V (or U Y0 V Y1): luma every 2 bytes, chroma every 4 bytes, full height.
                let (yo, uo, vo) = if matches!(raw, RawFrame::Yuyv { .. }) { (0, 1, 3) } else { (1, 0, 2) };
                if same {
                    packed422_full(data, stride, [yo, uo, vo], out);
                } else {
                    let (yp, up, vp) = out.planes_mut();
                    self.plane(Plane::of(data, yo, stride, 2, sw, sh), yp, dw, dh, None);
                    self.plane(Plane::of(data, uo, stride, 4, scw, sh), up, cw, ch, None);
                    self.plane(Plane::of(data, vo, stride, 4, scw, sh), vp, cw, ch, None);
                }
            }
            RawFrame::YccFull { data, .. } => {
                let (yp, up, vp) = out.planes_mut();
                self.plane(Plane::of(data, 0, sw * 3, 3, sw, sh), yp, dw, dh, Some(&FULL_TO_LIMITED_Y));
                // Chroma is full resolution here; the filter averages it down to 4:2:0.
                self.plane(Plane::of(data, 1, sw * 3, 3, sw, sh), up, cw, ch, Some(&FULL_TO_LIMITED_C));
                self.plane(Plane::of(data, 2, sw * 3, 3, sw, sh), vp, cw, ch, Some(&FULL_TO_LIMITED_C));
            }
        }
    }

    /// Scales an I420 picture into `out` (whose size is already set).
    fn i420(&mut self, v: &YuvView<'_>, out: &mut I420Frame) {
        let (dw, dh) = (out.width() as usize, out.height() as usize);
        let (scw, sch) = (v.width.div_ceil(2), v.height.div_ceil(2));
        let (yp, up, vp) = out.planes_mut();
        self.plane(Plane::of(v.y, 0, v.stride_y, 1, v.width, v.height), yp, dw, dh, None);
        self.plane(Plane::of(v.u, 0, v.stride_uv, 1, scw, sch), up, dw / 2, dh / 2, None);
        self.plane(Plane::of(v.v, 0, v.stride_uv, 1, scw, sch), vp, dw / 2, dh / 2, None);
    }

    fn taps(&mut self, src: usize, dst: usize) -> usize {
        if let Some(i) = self.taps.iter().position(|t| t.src == src && t.dst == dst) {
            return i;
        }
        if self.taps.len() >= 8 {
            self.taps.remove(0);
        }
        self.taps.push(Taps::new(src, dst));
        self.taps.len() - 1
    }

    /// Resamples one plane to `dw`x`dh` (tightly packed in `dst`), optionally remapping values.
    fn plane(&mut self, src: Plane<'_>, dst: &mut [u8], dw: usize, dh: usize, lut: Option<&[u8; 256]>) {
        let (sw, sh) = (src.width, src.height);
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
            return;
        }
        let same_w = dw == sw || dw == sw & !1;
        let same_h = dh == sh || dh == sh & !1;
        if same_w && same_h {
            for (y, out) in dst.chunks_exact_mut(dw).enumerate().take(dh) {
                copy_row(src.row(y), src.step, out);
                if let Some(lut) = lut {
                    out.iter_mut().for_each(|v| *v = lut[*v as usize]);
                }
            }
            return;
        }
        let (hi, vi) =
            (if same_w { usize::MAX } else { self.taps(sw, dw) }, if same_h { usize::MAX } else { self.taps(sh, dh) });
        let mut acc = std::mem::take(&mut self.acc);
        acc.clear();
        acc.resize(sw, 0);
        for oy in 0..dh {
            let out = &mut dst[oy * dw..(oy + 1) * dw];
            // Vertical pass into `acc` (values scaled by ONE).
            if same_h {
                let row = src.row(oy);
                if row.is_empty() {
                    acc.fill(0);
                } else {
                    for (a, &p) in acc.iter_mut().zip(row.iter().step_by(src.step)) {
                        *a = (p as u32) << 14;
                    }
                }
            } else {
                let t = &self.taps[vi];
                let (s, ws) = (t.start[oy] as usize, &t.w[oy * t.k..(oy + 1) * t.k]);
                let mut first = true;
                for (j, &w) in ws.iter().enumerate() {
                    if w == 0 {
                        continue;
                    }
                    let row = src.row(s + j);
                    let w = w as u32;
                    if row.is_empty() {
                        if first {
                            acc.fill(0);
                        }
                    } else if src.step == 1 {
                        if first {
                            acc.iter_mut().zip(row).for_each(|(a, &p)| *a = w * p as u32);
                        } else {
                            acc.iter_mut().zip(row).for_each(|(a, &p)| *a += w * p as u32);
                        }
                    } else if first {
                        acc.iter_mut().zip(row.iter().step_by(src.step)).for_each(|(a, &p)| *a = w * p as u32);
                    } else {
                        acc.iter_mut().zip(row.iter().step_by(src.step)).for_each(|(a, &p)| *a += w * p as u32);
                    }
                    first = false;
                }
            }
            // Horizontal pass.
            if same_w {
                for (o, &a) in out.iter_mut().zip(acc.iter()) {
                    *o = ((a + (ONE >> 1)) >> 14) as u8;
                }
            } else {
                let t = &self.taps[hi];
                match t.k {
                    1 => hpass::<1>(&acc, t, out),
                    2 => hpass::<2>(&acc, t, out),
                    3 => hpass::<3>(&acc, t, out),
                    4 => hpass::<4>(&acc, t, out),
                    5 => hpass::<5>(&acc, t, out),
                    _ => hpass_n(&acc, t, out),
                }
            }
            if let Some(lut) = lut {
                out.iter_mut().for_each(|v| *v = lut[*v as usize]);
            }
        }
        self.acc = acc;
    }
}

fn copy_row(row: &[u8], step: usize, out: &mut [u8]) {
    if row.is_empty() {
        out.fill(0);
    } else if step == 1 {
        let n = out.len().min(row.len());
        out[..n].copy_from_slice(&row[..n]);
    } else {
        for (o, &p) in out.iter_mut().zip(row.iter().step_by(step)) {
            *o = p;
        }
    }
}

#[inline]
fn hpass<const K: usize>(acc: &[u32], t: &Taps, out: &mut [u8]) {
    for ((o, &s), w) in out.iter_mut().zip(&t.start).zip(t.w.as_chunks::<K>().0.iter()) {
        let a = &acc[s as usize..s as usize + K];
        let mut sum = 0u32;
        for j in 0..K {
            sum += w[j] as u32 * (a[j] >> 6);
        }
        *o = ((sum + (1 << 21)) >> 22) as u8;
    }
}

fn hpass_n(acc: &[u32], t: &Taps, out: &mut [u8]) {
    for ((o, &s), w) in out.iter_mut().zip(&t.start).zip(t.w.chunks_exact(t.k)) {
        let a = &acc[s as usize..s as usize + t.k];
        let sum: u64 = w.iter().zip(a).map(|(&w, &a)| w as u64 * a as u64).sum();
        *o = ((sum + (1 << 27)) >> 28) as u8;
    }
}

/// RGB(A) -> I420 at the same size (cropping a trailing odd row/column). `out` is already sized.
fn rgb_to_i420_full(data: &[u8], stride: usize, bgr: bool, out: &mut I420Frame) {
    if bgr { rgb_full::<true>(data, stride, out) } else { rgb_full::<false>(data, stride, out) }
}

fn rgb_full<const BGR: bool>(data: &[u8], stride: usize, out: &mut I420Frame) {
    let w = out.width() as usize;
    let (yp, up, vp) = out.planes_mut();
    let (ri, bi) = if BGR { (2, 0) } else { (0, 2) };
    let rows = yp.chunks_exact_mut(w * 2).zip(up.chunks_exact_mut(w / 2)).zip(vp.chunks_exact_mut(w / 2));
    for (cy, ((yy, ur), vr)) in rows.enumerate() {
        let (y0, y1) = yy.split_at_mut(w);
        let r0 = data.get(2 * cy * stride..2 * cy * stride + w * 4).unwrap_or(&[]);
        let r1 = data.get((2 * cy + 1) * stride..(2 * cy + 1) * stride + w * 4).unwrap_or(&[]);
        if r0.is_empty() || r1.is_empty() {
            continue;
        }
        let it = r0
            .as_chunks::<8>()
            .0
            .iter()
            .zip(r1.as_chunks::<8>().0.iter())
            .zip(y0.as_chunks_mut::<2>().0.iter_mut())
            .zip(y1.as_chunks_mut::<2>().0.iter_mut());
        for ((((p, q), ya), yb), (u, v)) in it.zip(ur.iter_mut().zip(vr.iter_mut())) {
            let px = |s: &[u8], o: usize| [s[o + ri] as i32, s[o + 1] as i32, s[o + bi] as i32];
            let (a, b, c, d) = (px(p, 0), px(p, 4), px(q, 0), px(q, 4));
            ya[0] = luma(&a);
            ya[1] = luma(&b);
            yb[0] = luma(&c);
            yb[1] = luma(&d);
            let (cu, cv) = chroma4([a[0] + b[0] + c[0] + d[0], a[1] + b[1] + c[1] + d[1], a[2] + b[2] + c[2] + d[2]]);
            *u = cu;
            *v = cv;
        }
    }
}

/// YUYV / UYVY -> I420 at the same size. `o` = byte offsets of Y0, U and V in each 4-byte group.
fn packed422_full(data: &[u8], stride: usize, o: [usize; 3], out: &mut I420Frame) {
    let w = out.width() as usize;
    let (yp, up, vp) = out.planes_mut();
    let rows = yp.chunks_exact_mut(w * 2).zip(up.chunks_exact_mut(w / 2)).zip(vp.chunks_exact_mut(w / 2));
    for (cy, ((yy, ur), vr)) in rows.enumerate() {
        let (y0, y1) = yy.split_at_mut(w);
        let r0 = data.get(2 * cy * stride..2 * cy * stride + w * 2).unwrap_or(&[]);
        let r1 = data.get((2 * cy + 1) * stride..(2 * cy + 1) * stride + w * 2).unwrap_or(&[]);
        if r0.is_empty() || r1.is_empty() {
            continue;
        }
        let it = r0
            .as_chunks::<4>()
            .0
            .iter()
            .zip(r1.as_chunks::<4>().0.iter())
            .zip(y0.as_chunks_mut::<2>().0.iter_mut())
            .zip(y1.as_chunks_mut::<2>().0.iter_mut());
        for ((((p, q), ya), yb), (u, v)) in it.zip(ur.iter_mut().zip(vr.iter_mut())) {
            ya[0] = p[o[0]];
            ya[1] = p[o[0] + 2];
            yb[0] = q[o[0]];
            yb[1] = q[o[0] + 2];
            *u = ((p[o[1]] as u32 + q[o[1]] as u32 + 1) >> 1) as u8;
            *v = ((p[o[2]] as u32 + q[o[2]] as u32 + 1) >> 1) as u8;
        }
    }
}

// BT.709 limited range, 8.8 fixed point (libyuv's coefficients).
#[inline]
fn luma(p: &[i32; 3]) -> u8 {
    (((47 * p[0] + 157 * p[1] + 16 * p[2] + 128) >> 8) + 16) as u8
}

/// U and V from the sum of four RGB pixels.
#[inline]
fn chroma4(s: [i32; 3]) -> (u8, u8) {
    let u = ((-26 * s[0] - 87 * s[1] + 112 * s[2] + 512) >> 10) + 128;
    let v = ((112 * s[0] - 102 * s[1] - 10 * s[2] + 512) >> 10) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

#[cfg_attr(any(target_os = "macos", not(feature = "capture")), allow(dead_code))] // macOS draws the cursor itself
#[inline]
pub(crate) fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let p = [r as i32, g as i32, b as i32];
    let (u, v) = chroma4([p[0] * 4, p[1] * 4, p[2] * 4]);
    (luma(&p), u, v)
}

// ---------------------------------------------------------------------------------------------
// Cursor overlay

#[cfg_attr(any(target_os = "macos", not(feature = "capture")), allow(dead_code))] // macOS draws the cursor itself
/// A mouse pointer image to draw onto captured frames (straight alpha RGBA).
#[derive(Clone, Debug, Default)]
pub(crate) struct CursorImage {
    pub width: usize,
    pub height: usize,
    pub hot_x: i32,
    pub hot_y: i32,
    pub rgba: Vec<u8>,
    /// Per pixel: invert the screen instead of blending (Windows monochrome cursors such as the
    /// text I-beam). Empty when unused.
    pub invert: Vec<bool>,
}

#[cfg_attr(any(target_os = "macos", not(feature = "capture")), allow(dead_code))] // macOS draws the cursor itself
/// Draws `cur` onto `out` with its hotspot at (`x`, `y`) in source pixels, where the source was
/// `src_w`x`src_h` before scaling to the frame size.
pub(crate) fn overlay_cursor(out: &mut I420Frame, cur: &CursorImage, x: i32, y: i32, src_w: usize, src_h: usize) {
    if cur.width == 0 || cur.height == 0 || cur.rgba.len() < cur.width * cur.height * 4 {
        return;
    }
    let (fw, fh) = (out.width() as i64, out.height() as i64);
    let (sx, sy) = (fw as f64 / src_w.max(1) as f64, fh as f64 / src_h.max(1) as f64);
    let left = ((x - cur.hot_x) as f64 * sx).floor() as i64;
    let top = ((y - cur.hot_y) as f64 * sy).floor() as i64;
    let (cw, ch) = (((cur.width as f64 * sx).ceil() as i64).max(1), ((cur.height as f64 * sy).ceil() as i64).max(1));
    let w = fw as usize;
    let (yp, up, vp) = out.planes_mut();
    for oy in top.max(0)..(top + ch).min(fh) {
        let cy = (((oy - top) as f64 / sy) as usize).min(cur.height - 1);
        for ox in left.max(0)..(left + cw).min(fw) {
            let cx = (((ox - left) as f64 / sx) as usize).min(cur.width - 1);
            let i = cy * cur.width + cx;
            let yi = oy as usize * w + ox as usize;
            if cur.invert.get(i) == Some(&true) {
                yp[yi] = 251u8.saturating_sub(yp[yi]).max(16);
                continue;
            }
            let px = &cur.rgba[i * 4..i * 4 + 4];
            let a = px[3] as u32;
            if a == 0 {
                continue;
            }
            let (cy_, cu, cv) = rgb_to_yuv(px[0], px[1], px[2]);
            let blend = |dst: u8, src: u8| ((src as u32 * a + dst as u32 * (255 - a) + 127) / 255) as u8;
            yp[yi] = blend(yp[yi], cy_);
            if ox % 2 == 0 && oy % 2 == 0 {
                let ci = (oy as usize / 2) * (w / 2) + ox as usize / 2;
                up[ci] = blend(up[ci], cu);
                vp[ci] = blend(vp[ci], cv);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// I420 -> RGBA (display)

/// A borrowed YUV 4:2:0 picture with strides (decoder output or an [`I420Frame`]).
#[derive(Clone, Copy)]
pub(crate) struct YuvView<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride_y: usize,
    pub stride_uv: usize,
}

impl<'a> From<&'a I420Frame> for YuvView<'a> {
    fn from(f: &'a I420Frame) -> Self {
        let w = f.width() as usize;
        YuvView { y: f.y(), u: f.u(), v: f.v(), width: w, height: f.height() as usize, stride_y: w, stride_uv: w / 2 }
    }
}

/// Converts `pic` to RGBA8 at `ow`x`oh` (rounded down to even) into `out` (resized as needed),
/// area-averaging when scaling down. BT.709 limited range. Returns the size written.
pub(crate) fn to_rgba(pic: &YuvView<'_>, ow: u32, oh: u32, out: &mut Vec<u8>) -> (u32, u32) {
    thread_local!(static SCRATCH: std::cell::RefCell<(Converter, I420Frame)> = Default::default());
    let (ow, oh) = ((ow & !1).max(2), (oh & !1).max(2));
    out.clear();
    out.resize(ow as usize * oh as usize * 4, 255);
    if ow as usize == pic.width & !1 && oh as usize == pic.height & !1 {
        yuv_full_to_rgba(pic, ow as usize, oh as usize, out);
    } else {
        SCRATCH.with(|s| {
            let (conv, small) = &mut *s.borrow_mut();
            small.resize(ow, oh);
            conv.i420(pic, small);
            yuv_full_to_rgba(&YuvView::from(&*small), ow as usize, oh as usize, out);
        });
    }
    (ow, oh)
}

fn yuv_full_to_rgba(pic: &YuvView<'_>, w: usize, h: usize, out: &mut [u8]) {
    for (y, row) in out.chunks_exact_mut(w * 4).enumerate().take(h) {
        let yr = &pic.y[y * pic.stride_y..][..w];
        let ur = &pic.u[(y / 2) * pic.stride_uv..][..w / 2];
        let vr = &pic.v[(y / 2) * pic.stride_uv..][..w / 2];
        for (((px, ys), &u), &v) in
            row.as_chunks_mut::<8>().0.iter_mut().zip(yr.as_chunks::<2>().0.iter()).zip(ur).zip(vr)
        {
            let (u, v) = (u as i32 - 128, v as i32 - 128);
            let (cr, cg, cb) = (RV * v, GU * u + GV * v, BU * u);
            for (p, &yv) in px.as_chunks_mut::<4>().0.iter_mut().zip(ys) {
                let yy = (yv as i32 - 16) * Y_MUL + 2048;
                p[0] = ((yy + cr) >> 12).clamp(0, 255) as u8;
                p[1] = ((yy - cg) >> 12).clamp(0, 255) as u8;
                p[2] = ((yy + cb) >> 12).clamp(0, 255) as u8;
            }
        }
    }
}

// BT.709 limited range, 12-bit fixed point.
const Y_MUL: i32 = 4769; // 255/219
const RV: i32 = 7343; // 2(1-Kr) * 255/224
const GU: i32 = 873;
const GV: i32 = 2183;
const BU: i32 = 8652;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_sizes() {
        assert_eq!(fit_size(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_size(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit_size(1080, 1920, 1280, 720), (404, 720));
        assert_eq!(fit_size(640, 480, 1280, 720), (640, 480));
        assert_eq!(fit_size(641, 481, 1280, 720), (640, 480));
        assert_eq!(fit_size(0, 0, 100, 100), (2, 2));
        assert_eq!(fit_side(1920, 1080, 640), (640, 360));
    }

    #[test]
    fn range_luts() {
        assert_eq!(FULL_TO_LIMITED_Y[0], 16);
        assert_eq!(FULL_TO_LIMITED_Y[255], 235);
        assert_eq!(FULL_TO_LIMITED_C[128], 128);
        assert_eq!(FULL_TO_LIMITED_C[0], 16);
        assert_eq!(FULL_TO_LIMITED_C[255], 240);
    }

    fn solid_bgra(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [rgb[2], rgb[1], rgb[0], 255]).collect()
    }

    #[test]
    fn rgb_roundtrip_colors() {
        let mut c = Converter::default();
        for rgb in [[255, 255, 255], [0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255], [40, 120, 200]] {
            for (w, h, mw, mh) in [(8, 6, 8, 6), (37, 23, 16, 16)] {
                let src = solid_bgra(w, h, rgb);
                let mut f = I420Frame::default();
                c.convert(&RawFrame::Bgra { data: &src, stride: w * 4, width: w, height: h }, mw, mh, &mut f);
                let mut out = Vec::new();
                to_rgba(&YuvView::from(&f), f.width(), f.height(), &mut out);
                for px in out.chunks(4) {
                    for k in 0..3 {
                        assert!((px[k] as i32 - rgb[k] as i32).abs() <= 3, "{rgb:?} -> {px:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn packed_yuv_formats_agree() {
        // A horizontal luma ramp with constant chroma, as YUYV, UYVY, NV12, I420 and JPEG YCbCr.
        let (w, h) = (64usize, 32usize);
        let yv = |x: usize| (16 + x * 3) as u8;
        let yuyv: Vec<u8> =
            (0..h).flat_map(|_| (0..w / 2).flat_map(|i| [yv(2 * i), 100, yv(2 * i + 1), 160])).collect();
        let uyvy: Vec<u8> =
            (0..h).flat_map(|_| (0..w / 2).flat_map(|i| [100, yv(2 * i), 160, yv(2 * i + 1)])).collect();
        let yplane: Vec<u8> = (0..h).flat_map(|_| (0..w).map(yv)).collect();
        let uv: Vec<u8> = (0..w * h / 2).map(|i| if i % 2 == 0 { 100 } else { 160 }).collect();
        let (u, v) = (vec![100u8; w * h / 4], vec![160u8; w * h / 4]);
        let mut c = Converter::default();
        let mut frames = Vec::new();
        for raw in [
            RawFrame::Yuyv { data: &yuyv, stride: w * 2, width: w, height: h },
            RawFrame::Uyvy { data: &uyvy, stride: w * 2, width: w, height: h },
            RawFrame::Nv12 { y: &yplane, y_stride: w, uv: &uv, uv_stride: w, width: w, height: h },
            RawFrame::I420 { y: &yplane, u: &u, v: &v, y_stride: w, uv_stride: w / 2, width: w, height: h },
        ] {
            for (mw, mh) in [(64, 32), (32, 16), (20, 20)] {
                let mut f = I420Frame::default();
                c.convert(&raw, mw, mh, &mut f);
                frames.push(((mw, mh), f));
            }
        }
        for ((size, f), (size2, g)) in frames.iter().zip(frames.iter().skip(3)) {
            if size == size2 {
                assert_eq!(f, g);
            }
        }
        assert!(frames[0].1.u().iter().all(|&x| x == 100) && frames[0].1.v().iter().all(|&x| x == 160));
        assert_eq!(&frames[0].1.y()[..4], &[16, 19, 22, 25]);
        // Half size averages pairs.
        assert_eq!(frames[1].1.width(), 32);
        assert_eq!(frames[1].1.y()[0], 18);

        // Full-range YCbCr (JPEG) gets mapped to limited range.
        let ycc: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 128, 128]).collect();
        let mut f = I420Frame::default();
        c.convert(&RawFrame::YccFull { data: &ycc, width: w, height: h }, 32, 32, &mut f);
        assert!(f.y().iter().all(|&y| y == 235) && f.u().iter().all(|&u| u == 128));
    }

    #[test]
    fn cursor_overlay_draws_and_clips() {
        let mut f = I420Frame::new(16, 16);
        let cur = CursorImage { width: 4, height: 4, hot_x: 1, hot_y: 1, rgba: vec![255; 64], invert: vec![] };
        overlay_cursor(&mut f, &cur, 1, 1, 16, 16);
        assert_eq!(f.y()[0], 235);
        assert_eq!(f.y()[3 * 16 + 3], 235);
        assert_eq!(f.y()[4 * 16 + 4], 16);
        // Partly off screen, scaled down 2x: must not panic.
        overlay_cursor(&mut f, &cur, 31, -2, 32, 32);
        overlay_cursor(&mut f, &cur, -100, 1000, 32, 32);
    }
}

/// `cargo test --release -p celerity-stream --lib conversion_speed -- --ignored --nocapture`
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    fn time(name: &str, n: usize, mut f: impl FnMut()) {
        f();
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        eprintln!("{name}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }

    #[test]
    #[ignore]
    fn conversion_speed() {
        let mut c = Converter::default();
        let mut out = I420Frame::default();
        let (w, h) = (1920usize, 1080usize);
        let bgra: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let nv12: Vec<u8> = (0..w * h * 3 / 2).map(|i| (i * 7 % 251) as u8).collect();
        let yuyv: Vec<u8> = (0..w * h * 2).map(|i| (i * 7 % 251) as u8).collect();
        for (mw, mh) in [(1920, 1080), (1280, 720), (640, 360)] {
            time(&format!("bgra 1080p -> {mw}x{mh}"), 20, || {
                c.convert(&RawFrame::Bgra { data: &bgra, stride: w * 4, width: w, height: h }, mw, mh, &mut out)
            });
            time(&format!("nv12 1080p -> {mw}x{mh}"), 20, || {
                let (y, uv) = nv12.split_at(w * h);
                c.convert(&RawFrame::Nv12 { y, y_stride: w, uv, uv_stride: w, width: w, height: h }, mw, mh, &mut out)
            });
            time(&format!("yuyv 1080p -> {mw}x{mh}"), 20, || {
                c.convert(&RawFrame::Yuyv { data: &yuyv, stride: w * 2, width: w, height: h }, mw, mh, &mut out)
            });
        }
        let f = I420Frame::new(1280, 720);
        let mut rgba = Vec::new();
        time("i420 720p -> rgba 720p", 20, || {
            to_rgba(&YuvView::from(&f), 1280, 720, &mut rgba);
        });
        time("i420 720p -> rgba 640x360", 20, || {
            to_rgba(&YuvView::from(&f), 640, 360, &mut rgba);
        });
    }
}
