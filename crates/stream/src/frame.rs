//! Frame types shared by capture, the encoder and the decoder.

use std::sync::Arc;

/// A YUV 4:2:0 planar picture (I420: Y plane, then U, then V), tightly packed, BT.709 limited
/// range. Width and height are always even (at least 2x2), which keeps chroma planes exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct I420Frame {
    width: u32,
    height: u32,
    data: Vec<u8>,
    /// Capture time in microseconds on a monotonic clock (see [`crate::now_us`]).
    pub timestamp_us: u64,
}

impl I420Frame {
    /// A black frame. Odd sizes are rounded down to even (minimum 2x2).
    pub fn new(width: u32, height: u32) -> Self {
        let mut f = I420Frame::default();
        f.resize(width, height);
        f
    }

    /// Changes the size, reusing the allocation. Contents are unspecified afterwards (black when
    /// the buffer grew).
    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (even(width), even(height));
        self.width = w;
        self.height = h;
        let ys = (w * h) as usize;
        let len = ys + ys / 2;
        if self.data.len() != len {
            self.data.clear();
            self.data.resize(ys, 16);
            self.data.resize(len, 128);
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row of the Y plane (always `width`: planes are tightly packed).
    pub fn y_stride(&self) -> u32 {
        self.width
    }

    /// Bytes per row of the U and V planes (`width / 2`).
    pub fn uv_stride(&self) -> u32 {
        self.width / 2
    }

    /// All three planes back to back (`width * height * 3 / 2` bytes).
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn y(&self) -> &[u8] {
        &self.data[..self.luma_len()]
    }

    pub fn u(&self) -> &[u8] {
        let (l, c) = (self.luma_len(), self.luma_len() / 4);
        &self.data[l..l + c]
    }

    pub fn v(&self) -> &[u8] {
        let (l, c) = (self.luma_len(), self.luma_len() / 4);
        &self.data[l + c..]
    }

    /// Mutable Y, U and V planes.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let l = self.luma_len();
        let (y, uv) = self.data.split_at_mut(l);
        let (u, v) = uv.split_at_mut(l / 4);
        (y, u, v)
    }

    /// Copies `other` into `self`, reusing the allocation.
    pub fn copy_from(&mut self, other: &I420Frame) {
        self.width = other.width;
        self.height = other.height;
        self.timestamp_us = other.timestamp_us;
        self.data.clear();
        self.data.extend_from_slice(&other.data);
    }

    fn luma_len(&self) -> usize {
        (self.width * self.height) as usize
    }
}

fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

/// A decoded (or preview) picture: tightly packed RGBA8 (`width * height * 4` bytes, alpha 255)
/// by default, or the I420 planes when the receiver was asked for [`Output::I420`] (for a GPU
/// shader doing the colour conversion). Same shape as `celerity_video::Frame` in RGBA mode, so
/// the GUI can upload both the same way.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// RGBA8 pixels; empty when the frame carries [`Frame::i420`] instead.
    pub rgba: Arc<Vec<u8>>,
    /// The picture as I420 (BT.709 limited range) with [`Output::I420`], at `width`x`height`:
    /// Y is `width` bytes per row, U and V `width / 2` (see [`I420Frame::y_stride`]).
    pub i420: Option<Arc<I420Frame>>,
    /// Increases whenever a different picture is handed out, so a UI can skip re-uploading a
    /// texture it already has.
    pub serial: u64,
    /// Sender timestamp of this picture (milliseconds, sender's clock, wraps).
    pub timestamp_ms: u32,
}

impl Frame {
    /// The pixels as one of the two layouts.
    pub fn pixels(&self) -> Pixels<'_> {
        match &self.i420 {
            Some(f) => Pixels::I420 {
                y: f.y(),
                u: f.u(),
                v: f.v(),
                y_stride: f.y_stride() as usize,
                uv_stride: f.uv_stride() as usize,
            },
            None => Pixels::Rgba(&self.rgba),
        }
    }
}

/// Borrowed pixels of a [`Frame`].
#[derive(Clone, Copy, Debug)]
pub enum Pixels<'a> {
    /// `width * height * 4` bytes.
    Rgba(&'a [u8]),
    /// Planar 4:2:0: chroma planes are half the width and height (both are even).
    I420 { y: &'a [u8], u: &'a [u8], v: &'a [u8], y_stride: usize, uv_stride: usize },
}

/// What a [`crate::Receiver`] hands out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Output {
    /// RGBA8, converted (and scaled) on the decoder thread: for software rendering.
    #[default]
    Rgba,
    /// The decoded I420 planes (scaled only when larger than `max_side`): upload the three
    /// planes as R8 textures and convert in a shader. At 1440p60 this saves the RGBA
    /// conversion and 60% of the upload bandwidth.
    I420,
}

/// What a stream carries; picks encoder tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentKind {
    /// Facecam: natural video, motion matters.
    Camera,
    /// Screen share: sharp text matters most, then smooth motion.
    Screen,
}
