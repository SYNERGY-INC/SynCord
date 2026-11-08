//! A synthetic moving picture, for tests, benchmarks and GUI work without a camera.

use crate::frame::I420Frame;

/// Renders frame `n` of a moving test pattern into `out` (keeps its size): a slowly shifting
/// colour gradient, a bouncing white square and a row of fine stripes (text-like detail).
pub fn render_test_pattern(n: u64, out: &mut I420Frame) {
    let (w, h) = (out.width() as usize, out.height() as usize);
    let (yp, up, vp) = out.planes_mut();
    let t = n as usize;
    for y in 0..h {
        let row = &mut yp[y * w..(y + 1) * w];
        for (x, p) in row.iter_mut().enumerate() {
            *p = (16 + ((x + y + t * 2) * 200 / (w + h)) % 200) as u8;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            up[y * (w / 2) + x] = (64 + (x * 128 / (w / 2).max(1) + t) % 128) as u8;
            vp[y * (w / 2) + x] = (64 + (y * 128 / (h / 2).max(1) + t / 2) % 128) as u8;
        }
    }
    // Fine stripes near the top, like a line of text.
    for y in h / 10..(h / 10 + h / 30).min(h) {
        for x in (w / 10..w * 9 / 10).step_by(3) {
            yp[y * w + x] = 235;
        }
    }
    // Bouncing square, 1/6 of the height.
    let s = (h / 6).max(2);
    let path = |pos: usize, span: usize| {
        let span = span.max(1);
        let p = pos % (2 * span);
        if p < span { p } else { 2 * span - p }
    };
    let bx = path(t * 7, w.saturating_sub(s));
    let by = path(t * 5, h.saturating_sub(s));
    for y in by..(by + s).min(h) {
        yp[y * w + bx..y * w + (bx + s).min(w)].fill(235);
    }
}

/// Mean absolute difference between the luma planes of two same-sized frames (0 = identical).
/// Handy for checking decoded output against what was sent.
pub fn luma_difference(a: &I420Frame, b: &I420Frame) -> f64 {
    if a.width() != b.width() || a.height() != b.height() {
        return f64::INFINITY;
    }
    let sum: u64 = a.y().iter().zip(b.y()).map(|(&p, &q)| (p as i32 - q as i32).unsigned_abs() as u64).sum();
    sum as f64 / a.y().len().max(1) as f64
}
