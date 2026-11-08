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

/// A decoded (or preview) frame as tightly packed RGBA8 (`width * height * 4` bytes, alpha 255).
/// Same shape as `syncord_video::Frame`, so the GUI can upload both the same way.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
    /// Increases whenever a different picture is handed out, so a UI can skip re-uploading a
    /// texture it already has.
    pub serial: u64,
    /// Sender timestamp of this picture (milliseconds, sender's clock, wraps).
    pub timestamp_ms: u32,
}

/// What a stream carries; picks encoder tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentKind {
    /// Facecam: natural video, motion matters, ~500-800 kbps.
    Camera,
    /// Screen share: sharp text matters more than frame rate, ~1.5-3 Mbps.
    Screen,
}
