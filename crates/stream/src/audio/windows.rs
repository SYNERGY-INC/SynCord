//! Windows: process loopback capture (WASAPI, Windows 10 2004+).

use super::{AudioSource, AudioSupport, Chunker};
use crate::capture::{Ready, Stop};

pub(crate) fn support() -> AudioSupport {
    AudioSupport::default()
}

pub(crate) fn run(_: AudioSource, _: Chunker, ready: Ready, _: &Stop) -> Result<(), String> {
    ready(Err("not yet".into()));
    Ok(())
}
