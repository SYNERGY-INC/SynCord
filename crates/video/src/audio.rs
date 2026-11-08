//! AAC decoding (symphonia) plus the small amount of DSP needed to feed an output device:
//! channel remixing and linear-interpolation resampling.

use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder as _, DecoderOptions};
use symphonia_core::formats::Packet;

use crate::demux::AudioTrack;

pub(crate) struct AacDecoder {
    dec: symphonia_codec_aac::AacDecoder,
    buf: Option<SampleBuffer<f32>>,
    pub channels: usize,
    pub rate: u32,
}

impl AacDecoder {
    pub fn new(track: &AudioTrack) -> Result<Self, String> {
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(track.sample_rate)
            .with_extra_data(track.asc.clone().into_boxed_slice());
        let dec = symphonia_codec_aac::AacDecoder::try_new(&params, &DecoderOptions::default())
            .map_err(|e| format!("AAC decoder: {e}"))?;
        Ok(AacDecoder { dec, buf: None, channels: track.channels.max(1) as usize, rate: track.sample_rate })
    }

    /// Decodes one packet to interleaved f32, returning (samples, channels, sample rate).
    /// A corrupt packet yields no samples (skip it).
    pub fn decode(&mut self, data: &[u8]) -> (&[f32], usize, u32) {
        let pkt = Packet::new_from_slice(0, 0, 0, data);
        let decoded = match self.dec.decode(&pkt) {
            Ok(d) => d,
            Err(_) => return (&[], self.channels, self.rate),
        };
        let spec = *decoded.spec();
        self.channels = spec.channels.count().max(1);
        self.rate = spec.rate;
        let cap = decoded.capacity() as u64;
        if self.buf.as_ref().is_none_or(|b| b.capacity() < cap as usize * self.channels) {
            self.buf = Some(SampleBuffer::new(cap, spec));
        }
        let buf = self.buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);
        (buf.samples(), self.channels, self.rate)
    }

    pub fn reset(&mut self) {
        self.dec.reset();
    }
}

/// Maps interleaved `input` with `in_ch` channels to `out_ch` channels, appending to `out`.
pub(crate) fn remix(input: &[f32], in_ch: usize, out_ch: usize, out: &mut Vec<f32>) {
    let frames = input.len() / in_ch.max(1);
    out.reserve(frames * out_ch);
    for f in input.chunks_exact(in_ch) {
        match (in_ch, out_ch) {
            (a, b) if a == b => out.extend_from_slice(f),
            (1, _) => out.extend(std::iter::repeat_n(f[0], out_ch)),
            (_, 1) => out.push(f.iter().sum::<f32>() / in_ch as f32),
            _ => {
                // Stereo (or more) to a different layout: keep the first channels, silence the rest.
                for c in 0..out_ch {
                    out.push(f.get(c).copied().unwrap_or(0.0));
                }
            }
        }
    }
}

/// Streaming linear-interpolation resampler for interleaved audio.
pub(crate) struct Resampler {
    from: u32,
    to: u32,
    ch: usize,
    /// Position of the next output frame, in input frames relative to `prev`.
    pos: f64,
    prev: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, ch: usize) -> Self {
        Resampler { from, to, ch, pos: 0.0, prev: vec![0.0; ch] }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev.iter_mut().for_each(|s| *s = 0.0);
    }

    pub fn set_rate(&mut self, from: u32) {
        self.from = from;
    }

    /// Resamples `input` (interleaved, `ch` channels) appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to || self.from == 0 {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.ch;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        let step = self.from as f64 / self.to as f64;
        // Virtual sequence: prev (index 0), then input frames (1..=frames).
        let at = |i: usize, c: usize| if i == 0 { self.prev[c] } else { input[(i - 1) * ch + c] };
        out.reserve(((frames as f64 / step) as usize + 1) * ch);
        while self.pos < frames as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = at(i, c);
                let b = at(i + 1, c);
                out.push(a + (b - a) * t);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        self.prev.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_length_and_continuity() {
        let mut r = Resampler::new(44100, 48000, 1);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..44100).map(|i| (i as f32 * 0.01).sin()).collect();
        for chunk in input.chunks(1024) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48000).abs() <= 2, "{}", out.len());
        // No discontinuities at chunk boundaries (signal is smooth).
        assert!(out.windows(2).skip(2).all(|w| (w[0] - w[1]).abs() < 0.02));
    }

    #[test]
    fn remix_layouts() {
        let mut out = Vec::new();
        remix(&[0.5, -0.5], 1, 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        remix(&[0.2, 0.4], 2, 1, &mut out);
        assert!((out[0] - 0.3).abs() < 1e-6);
        out.clear();
        remix(&[0.1, 0.2], 2, 4, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.0, 0.0]);
    }
}
