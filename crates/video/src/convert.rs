//! YUV 4:2:0 -> RGBA8 conversion fused with area-averaging downscale, so a full-resolution RGB
//! frame is never materialized.

use crate::h264::{Matrix, Picture};

/// Output size for a `w`x`h` picture so that the longest side is <= `max_side` (never upscales,
/// keeps aspect ratio, at least 1x1).
pub(crate) fn scaled_size(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let max_side = max_side.max(1);
    let long = w.max(h).max(1);
    if long <= max_side {
        return (w.max(1), h.max(1));
    }
    let sw = ((w as u64 * max_side as u64 + long as u64 / 2) / long as u64).clamp(1, max_side as u64);
    let sh = ((h as u64 * max_side as u64 + long as u64 / 2) / long as u64).clamp(1, max_side as u64);
    (sw as u32, sh as u32)
}

/// Fixed-point (12 bit) conversion coefficients.
struct Coeffs {
    y_mul: i32,
    y_off: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Coeffs {
    fn new(matrix: Matrix, full_range: bool) -> Self {
        let (kr, kb) = match matrix {
            Matrix::Bt601 => (0.299f32, 0.114f32),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if full_range { (1.0, 1.0, 0) } else { (255.0 / 219.0, 255.0 / 224.0, 16) };
        let f = |v: f32| (v * 4096.0).round() as i32;
        Coeffs {
            y_mul: f(ys),
            y_off: yo,
            rv: f(2.0 * (1.0 - kr) * cs),
            gu: f(2.0 * kb * (1.0 - kb) / kg * cs),
            gv: f(2.0 * kr * (1.0 - kr) / kg * cs),
            bu: f(2.0 * (1.0 - kb) * cs),
        }
    }
}

/// Source span [start, end) covered by each destination pixel along one axis.
fn spans(src: usize, dst: usize) -> Vec<(u32, u32)> {
    (0..dst)
        .map(|i| {
            let a = i * src / dst;
            let b = ((i + 1) * src / dst).max(a + 1).min(src);
            (a as u32, b as u32)
        })
        .collect()
}

/// Box-filters one plane into `avg` (one value per output column) for the source rows
/// `y0..y1`, reusing `acc` (column sums) and `recips` (cached reciprocals per row count).
#[allow(clippy::too_many_arguments)]
fn box_row(
    plane: &[u8],
    stride: usize,
    width: usize,
    (y0, y1): (u32, u32),
    xs: &[(u32, u32)],
    acc: &mut [u32],
    recips: &mut (u32, Vec<u64>),
    avg: &mut [i32],
) {
    let rows = y1 - y0;
    let first = &plane[y0 as usize * stride..][..width];
    for (a, &p) in acc.iter_mut().zip(first) {
        *a = p as u32;
    }
    for y in y0 + 1..y1 {
        let line = &plane[y as usize * stride..][..width];
        for (a, &p) in acc.iter_mut().zip(line) {
            *a += p as u32;
        }
    }
    if recips.0 != rows {
        // 32.32 fixed-point reciprocal of the box area, so the average is a multiply + shift.
        recips.0 = rows;
        recips.1.clear();
        recips.1.extend(xs.iter().map(|&(a, b)| {
            let n = (rows * (b - a)) as u64;
            ((1u64 << 32) + n / 2) / n
        }));
    }
    for ((o, &(a, b)), &r) in avg.iter_mut().zip(xs).zip(&recips.1) {
        let sum: u32 = acc[a as usize..b as usize].iter().sum();
        *o = ((sum as u64 * r + (1 << 31)) >> 32) as i32;
    }
}

/// Converts `pic` to RGBA8 at `out_w` x `out_h` into `out` (resized as needed), averaging the
/// source pixels covered by each output pixel (separable box filter), and rotating clockwise by
/// `rotation` degrees (0, 90, 180 or 270; `out_w` x `out_h` is the size after rotation).
pub(crate) fn to_rgba(
    pic: &Picture<'_>,
    out_w: u32,
    out_h: u32,
    rotation: u16,
    matrix: Matrix,
    full_range: bool,
    out: &mut Vec<u8>,
) {
    let (ow, oh) = (out_w.max(1) as usize, out_h.max(1) as usize);
    // Size before rotation.
    let (uw, uh) = if rotation % 180 == 90 { (oh, ow) } else { (ow, oh) };
    out.clear();
    out.resize(ow * oh * 4, 255);
    let c = Coeffs::new(matrix, full_range);
    let (cw, ch) = (pic.width.div_ceil(2), pic.height.div_ceil(2));
    let xs = spans(pic.width, uw);
    let ys = spans(pic.height, uh);
    // Chroma span for a luma span: [a/2, ceil(b/2)).
    let cxs: Vec<(u32, u32)> = xs.iter().map(|&(a, b)| (a / 2, b.div_ceil(2).min(cw as u32).max(a / 2 + 1))).collect();

    let mut acc_y = vec![0u32; pic.width];
    let mut acc_c = vec![0u32; cw];
    let (mut ry, mut rc) = ((0u32, Vec::new()), (0u32, Vec::new()));
    let (mut ya, mut ua, mut va) = (vec![0i32; uw], vec![0i32; uw], vec![0i32; uw]);
    let mut last_cy = (u32::MAX, u32::MAX);

    for (oy, &(y0, y1)) in ys.iter().enumerate() {
        let cy = (y0 / 2, y1.div_ceil(2).min(ch as u32).max(y0 / 2 + 1));
        box_row(pic.y, pic.stride_y, pic.width, (y0, y1), &xs, &mut acc_y, &mut ry, &mut ya);
        if cy != last_cy {
            // Chroma rows are shared by consecutive output rows when not downscaling much.
            last_cy = cy;
            box_row(pic.u, pic.stride_uv, cw, cy, &cxs, &mut acc_c, &mut rc, &mut ua);
            box_row(pic.v, pic.stride_uv, cw, cy, &cxs, &mut acc_c, &mut rc, &mut va);
        }
        let pixels = ya.iter().zip(&ua).zip(&va).map(|((&y, &u), &v)| c.rgb(y, u, v));
        if rotation == 0 {
            let row = &mut out[oy * ow * 4..(oy + 1) * ow * 4];
            for (px, rgb) in row.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
                px[..3].copy_from_slice(&rgb);
            }
        } else {
            for (ox, rgb) in pixels.enumerate() {
                let (dx, dy) = match rotation {
                    90 => (uh - 1 - oy, ox),
                    180 => (uw - 1 - ox, uh - 1 - oy),
                    _ => (oy, uw - 1 - ox), // 270
                };
                let i = (dy * ow + dx) * 4;
                out[i..i + 3].copy_from_slice(&rgb);
            }
        }
    }
}

impl Coeffs {
    #[inline]
    fn rgb(&self, y: i32, u: i32, v: i32) -> [u8; 3] {
        let (u, v) = (u - 128, v - 128);
        let yy = (y - self.y_off) * self.y_mul;
        let r = (yy + self.rv * v + 2048) >> 12;
        let g = (yy - self.gu * u - self.gv * v + 2048) >> 12;
        let b = (yy + self.bu * u + 2048) >> 12;
        [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_sizes() {
        assert_eq!(scaled_size(1920, 1080, 640), (640, 360));
        assert_eq!(scaled_size(1080, 1920, 640), (360, 640));
        assert_eq!(scaled_size(320, 240, 640), (320, 240));
        assert_eq!(scaled_size(321, 241, 100), (100, 75));
        assert_eq!(scaled_size(5000, 2, 100), (100, 1));
        assert_eq!(scaled_size(0, 0, 100), (1, 1));
    }

    #[test]
    fn gray_and_colors() {
        // 3x3 picture (odd size), limited range: Y=235 white, U=V=128.
        let y = [235u8; 9];
        let u = [128u8; 4];
        let v = [128u8; 4];
        let pic = Picture { y: &y, u: &u, v: &v, width: 3, height: 3, stride_y: 3, stride_uv: 2, ts: 0 };
        let mut out = Vec::new();
        to_rgba(&pic, 3, 3, 0, Matrix::Bt709, false, &mut out);
        assert_eq!(out.len(), 36);
        assert!(out.chunks(4).all(|p| p == [255, 255, 255, 255]));
        to_rgba(&pic, 1, 1, 0, Matrix::Bt601, false, &mut out);
        assert_eq!(out, [255, 255, 255, 255]);

        // Pure red in BT.601 limited: Y=81, U=90, V=240.
        let y = [81u8; 4];
        let u = [90u8; 1];
        let v = [240u8; 1];
        let pic = Picture { y: &y, u: &u, v: &v, width: 2, height: 2, stride_y: 2, stride_uv: 1, ts: 0 };
        to_rgba(&pic, 2, 2, 0, Matrix::Bt601, false, &mut out);
        let p = &out[0..4];
        assert!(p[0] > 250 && p[1] < 5 && p[2] < 5, "{p:?}");
    }

    #[test]
    fn rotation() {
        // 4x2 picture, white only at the top-left pixel (full range, no scaling).
        let y = [255, 0, 0, 0, 0, 0, 0, 0];
        let (u, v) = ([128u8; 2], [128u8; 2]);
        let pic = Picture { y: &y, u: &u, v: &v, width: 4, height: 2, stride_y: 4, stride_uv: 2, ts: 0 };
        let mut out = Vec::new();
        let white = |out: &[u8], w: usize| -> Vec<(usize, usize)> {
            (0..out.len() / 4).filter(|i| out[i * 4] > 128).map(|i| (i % w, i / w)).collect()
        };
        to_rgba(&pic, 4, 2, 0, Matrix::Bt601, true, &mut out);
        assert_eq!(white(&out, 4), [(0, 0)]);
        // Clockwise rotations; 90 and 270 swap the output size to 2x4.
        to_rgba(&pic, 2, 4, 90, Matrix::Bt601, true, &mut out);
        assert_eq!(white(&out, 2), [(1, 0)]);
        to_rgba(&pic, 4, 2, 180, Matrix::Bt601, true, &mut out);
        assert_eq!(white(&out, 4), [(3, 1)]);
        to_rgba(&pic, 2, 4, 270, Matrix::Bt601, true, &mut out);
        assert_eq!(white(&out, 2), [(0, 3)]);
        assert!(out.len() == 2 * 4 * 4 && out.chunks(4).all(|p| p[3] == 255));
    }
}
