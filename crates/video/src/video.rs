//! Sequential H.264 decoding of an MP4 video track with seeking.

use std::path::Path;

use crate::demux::{SampleReader, VideoTrack};
use crate::h264::{self, Picture};

pub(crate) enum Step<'a> {
    /// A decoded picture; `Picture::ts` is the index of the sample it came from.
    Picture(Picture<'a>),
    /// An access unit was consumed but no picture came out (yet).
    Nothing,
    /// All samples fed and the decoder is drained.
    End,
}

pub(crate) struct VideoDecoder {
    reader: SampleReader,
    dec: h264::Decoder,
    param_sets: Vec<u8>,
    annexb: Vec<u8>,
    next: usize,
    /// Consecutive failed/empty decodes, to give up on hopeless streams.
    misses: usize,
    flushes: usize,
}

impl VideoDecoder {
    pub fn new(path: &Path, video: &VideoTrack) -> Result<Self, String> {
        Ok(VideoDecoder {
            reader: SampleReader::open(path)?,
            dec: h264::Decoder::new()?,
            param_sets: h264::param_sets_annexb(&video.param_sets),
            annexb: Vec::new(),
            next: 0,
            misses: 0,
            flushes: 0,
        })
    }

    /// Restarts decoding at sample `idx` (should be a sync sample). A fresh decoder is used so no
    /// stale reference or reorder state survives the jump.
    pub fn restart_at(&mut self, idx: usize) -> Result<(), String> {
        self.dec = h264::Decoder::new()?;
        self.next = idx;
        self.misses = 0;
        self.flushes = 0;
        Ok(())
    }

    /// Too many access units in a row produced nothing: the stream is probably undecodable.
    pub fn hopeless(&self) -> bool {
        self.misses > 300
    }

    /// Feeds the next access unit (or drains the decoder at the end). Non-reference frames that
    /// end at or before `drop_before_ms` (track presentation time) are not decoded at all: callers
    /// pass the seek target or the current playback position so frames that would be thrown away
    /// anyway cost nothing. Pass `i64::MIN` to decode everything.
    pub fn step(&mut self, video: &VideoTrack, drop_before_ms: i64) -> Result<Step<'_>, String> {
        let samples = &video.track.samples;
        if self.next >= samples.len() {
            // The decoder holds a few pictures for reordering; drain them (bounded, in case it
            // keeps reporting pictures it can't hand out).
            self.flushes += 1;
            if self.flushes > 32 {
                return Ok(Step::End);
            }
            return Ok(match self.dec.flush() {
                Some(p) => Step::Picture(p),
                None => Step::End,
            });
        }
        let idx = self.next;
        self.next += 1;
        let s = samples[idx];
        self.annexb.clear();
        if s.sync {
            // Parameter sets live in avcC; repeating them before keyframes is harmless.
            self.annexb.extend_from_slice(&self.param_sets);
        }
        let data = self.reader.read(&s)?;
        if drop_before_ms > i64::MIN
            && !s.sync
            && video.track.to_ms(video.track.pts(idx) + s.dur as i64) <= drop_before_ms
            && h264::is_nonref(data, video.nal_len)
        {
            return Ok(Step::Nothing);
        }
        h264::append_annexb(data, video.nal_len, &mut self.annexb);
        if self.annexb.is_empty() {
            self.misses += 1;
            return Ok(Step::Nothing);
        }
        match self.dec.decode(&self.annexb, idx as u64) {
            Some(p) => {
                self.misses = 0;
                Ok(Step::Picture(p))
            }
            None => {
                self.misses += 1;
                Ok(Step::Nothing)
            }
        }
    }
}
