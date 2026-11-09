//! Works out what a soundboard upload is and how long it plays, from its headers alone
//! (nothing is decoded). Ogg (Opus or Vorbis), WAV and MP3 are accepted.

/// What an audio file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub mime: &'static str,
    pub duration_ms: u32,
}

/// Look at an uploaded sound. Errors are short, friendly reasons.
pub fn inspect(bytes: &[u8]) -> Result<AudioInfo, &'static str> {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        let ms = wav_duration(bytes).ok_or("that WAV file is damaged")?;
        return Ok(AudioInfo { mime: "audio/wav", duration_ms: ms });
    }
    if bytes.starts_with(b"OggS") {
        let ms = ogg_duration(bytes).ok_or("that Ogg file is damaged or isn't Opus or Vorbis")?;
        return Ok(AudioInfo { mime: "audio/ogg", duration_ms: ms });
    }
    if let Some(ms) = mp3_duration(bytes) {
        return Ok(AudioInfo { mime: "audio/mpeg", duration_ms: ms });
    }
    Err("use an Ogg (Opus or Vorbis), WAV or MP3 file")
}

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn ms(units: u64, per_second: u64) -> Option<u32> {
    (per_second > 0).then(|| (units.saturating_mul(1000) / per_second).min(u32::MAX as u64) as u32)
}

/// Data size divided by bytes per second, from the `fmt ` and `data` chunks.
fn wav_duration(b: &[u8]) -> Option<u32> {
    let (mut at, mut byte_rate, mut data) = (12usize, None, None);
    while at + 8 <= b.len() && (byte_rate.is_none() || data.is_none()) {
        let id = &b[at..at + 4];
        let size = u32le(b, at + 4)? as usize;
        match id {
            b"fmt " => byte_rate = u32le(b, at + 8 + 8),
            // Streams written on the fly can leave the size at 0 or "unknown": use what's there.
            b"data" => data = Some(if size == 0 || size == u32::MAX as usize { b.len() - at - 8 } else { size }),
            _ => {}
        }
        at = at.checked_add(8 + size + (size & 1))?;
    }
    ms(data? as u64, byte_rate? as u64)
}

/// The last page's granule position over the sample rate (48 kHz for Opus, minus its pre-skip).
fn ogg_duration(b: &[u8]) -> Option<u32> {
    // First page: its single packet says which codec it is.
    let segments = *b.get(26)? as usize;
    let payload = b.get(27 + segments..)?;
    let (rate, skip) = if payload.starts_with(b"OpusHead") {
        (48_000u64, u16le(payload, 10)? as u64)
    } else if payload.starts_with(b"\x01vorbis") {
        (u32le(payload, 12)? as u64, 0)
    } else {
        return None;
    };
    // Last page: search backwards for its capture pattern.
    let mut at = b.len().checked_sub(27)?;
    loop {
        if &b[at..at + 4] == b"OggS" && b[at + 4] == 0 {
            let granule = i64::from_le_bytes(b[at + 6..at + 14].try_into().ok()?);
            if granule >= 0 {
                return ms((granule as u64).saturating_sub(skip), rate);
            }
        }
        at = at.checked_sub(1)?;
    }
}

/// From the first frame's header: its Xing/Info frame count if there is one (variable
/// bitrate), otherwise the file size over the bitrate.
fn mp3_duration(b: &[u8]) -> Option<u32> {
    let mut at = 0;
    if b.starts_with(b"ID3") {
        let size = b.get(6..10)?.iter().fold(0usize, |n, x| n << 7 | (*x as usize & 0x7F));
        at = 10 + size;
    }
    let h = b.get(at..at + 4)?;
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5
    let layer = (h[1] >> 1) & 3; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    const MPEG1: [u64; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const MPEG2: [u64; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let bitrate = *if version == 3 { &MPEG1 } else { &MPEG2 }.get((h[2] >> 4) as usize)? * 1000;
    let rates: [u64; 3] = match version {
        3 => [44_100, 48_000, 32_000],
        2 => [22_050, 24_000, 16_000],
        _ => [11_025, 12_000, 8_000],
    };
    let rate = *rates.get(((h[2] >> 2) & 3) as usize)?;
    let samples_per_frame: u64 = if version == 3 { 1152 } else { 576 };
    // A Xing or Info header sits in the first frame, after the side information.
    let window = b.get(at + 4..(at + 4 + 64).min(b.len()))?;
    for tag in [b"Xing", b"Info"] {
        if let Some(i) = window.windows(4).position(|w| w == tag) {
            let flags = u32::from_be_bytes(window.get(i + 4..i + 8)?.try_into().ok()?);
            if flags & 1 != 0 {
                let frames = u32::from_be_bytes(window.get(i + 8..i + 12)?.try_into().ok()?) as u64;
                return ms(frames * samples_per_frame, rate);
            }
        }
    }
    if bitrate == 0 {
        return None;
    }
    ms((b.len() - at) as u64 * 8, bitrate)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A silent mono 16-bit WAV of `ms` milliseconds at 8 kHz.
    pub fn wav(ms: u32) -> Vec<u8> {
        let data = vec![0u8; (8_000 * 2 * ms / 1000) as usize];
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&8_000u32.to_le_bytes());
        out.extend_from_slice(&16_000u32.to_le_bytes()); // bytes per second
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    fn ogg_page(granule: i64, payload: &[u8]) -> Vec<u8> {
        let mut p = b"OggS\0\0".to_vec();
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&[0; 12]); // serial, sequence, checksum
        p.push(1);
        p.push(payload.len() as u8);
        p.extend_from_slice(payload);
        p
    }

    /// An Ogg Opus stream that says it lasts `ms` milliseconds.
    pub fn opus(ms: u32) -> Vec<u8> {
        let mut head = b"OpusHead\x01\x01".to_vec();
        head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut out = ogg_page(0, &head);
        out.extend(ogg_page(0, b"OpusTags"));
        out.extend(ogg_page(312 + ms as i64 * 48, &[0xF8; 40]));
        out
    }

    #[test]
    fn reads_lengths() {
        assert_eq!(inspect(&wav(1500)), Ok(AudioInfo { mime: "audio/wav", duration_ms: 1500 }));
        assert_eq!(inspect(&opus(4200)), Ok(AudioInfo { mime: "audio/ogg", duration_ms: 4200 }));
        // A 128 kbit/s MPEG-1 Layer III frame header followed by 2 seconds' worth of bytes.
        let mut mp3 = vec![0xFF, 0xFB, 0x90, 0x64];
        mp3.resize(32_000, 0);
        assert_eq!(inspect(&mp3), Ok(AudioInfo { mime: "audio/mpeg", duration_ms: 2000 }));
        assert!(inspect(b"definitely not audio").is_err());
        assert!(inspect(&wav(10)[..30]).is_err(), "cut-off header");
    }
}
