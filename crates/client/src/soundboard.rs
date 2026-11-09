//! The soundboard (0.7): a grid of the server's sounds in the call panel. Clicking one asks
//! the server to play it for everyone in the call; when the server says a sound was played,
//! it's downloaded once, decoded and played on your speakers at the sound effects volume.
//!
//! Sounds come as Ogg Opus (decoded with `opus`, after reading the Ogg pages here), or
//! Ogg Vorbis, WAV or MP3 (decoded with `symphonia`). Everything ends up as 48 kHz mono.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use celerity_proto::voice::SAMPLE_RATE;
use celerity_proto::{ClientMsg, ServerMsg, SoundId, perms};
use iced::widget::{button, column, container, row, text, tooltip};
use iced::{Alignment, Element, Length, Task};

use crate::app::{App, Message, Session};
use crate::icons;
use crate::style::{self, card};

/// Decoded sounds kept per server (`<server>/<id>`); a server has at most 100 short ones.
#[derive(Default)]
pub struct Soundboard {
    /// The grid in the call panel is showing.
    pub open: bool,
    cache: HashMap<String, Arc<Vec<f32>>>,
}

#[derive(Debug, Clone)]
pub enum SoundMsg {
    Toggle,
    /// Play it for everyone in the call.
    Play(SoundId),
    /// Listen to it yourself (in the settings).
    Preview(SoundId),
    Loaded(String, Result<Arc<Vec<f32>>, String>),
}

impl App {
    /// Whether the soundboard button shows: in a call on this server, with sounds to play
    /// (direct message and group calls may always use them).
    pub(crate) fn soundboard_available(&self, s: &Session) -> bool {
        let Some(v) = self.voice_here() else { return false };
        !s.sounds.is_empty() && (s.is_conversation(v.channel_id) || s.can(perms::USE_SOUNDBOARD))
    }

    pub(crate) fn update_soundboard(&mut self, msg: SoundMsg) -> Task<Message> {
        match msg {
            SoundMsg::Toggle => self.soundboard.open = !self.soundboard.open,
            SoundMsg::Play(sound_id) => self.send(ClientMsg::PlaySound { sound_id }),
            SoundMsg::Preview(id) => return self.play_sound(id),
            SoundMsg::Loaded(key, result) => match result {
                Ok(samples) => {
                    self.soundboard.cache.insert(key, samples.clone());
                    self.output_sound(samples);
                }
                Err(e) => eprintln!("couldn't play sound {key}: {e}"),
            },
        }
        Task::none()
    }

    pub(crate) fn on_sound_played(&mut self, msg: ServerMsg) -> Task<Message> {
        let ServerMsg::SoundPlayed { sound_id, .. } = msg else { return Task::none() };
        if self.deafened {
            return Task::none();
        }
        self.play_sound(sound_id)
    }

    /// Play a sound of this server, downloading it the first time.
    fn play_sound(&mut self, id: SoundId) -> Task<Message> {
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        let key = format!("{base}/{id}");
        if let Some(samples) = self.soundboard.cache.get(&key).cloned() {
            self.output_sound(samples);
            return Task::none();
        }
        let url = format!("{base}{}/{id}", celerity_proto::expressions::SOUNDS_PATH);
        Task::perform(
            async move {
                let resp = http.get(url).bearer_auth(token).send().await.map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("error {}", resp.status().as_u16()));
                }
                let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
                tokio::task::spawn_blocking(move || decode(&bytes)).await.map_err(|e| e.to_string())?.map(Arc::new)
            },
            move |r| Message::Soundboard(SoundMsg::Loaded(key.clone(), r)),
        )
    }

    /// On the chosen speakers at the sound effects volume.
    fn output_sound(&self, samples: Arc<Vec<f32>>) {
        let volume = self.settings.sound_volume;
        if volume <= 0.0 || self.deafened {
            return;
        }
        let device = self.settings.output_device.clone();
        let length = Duration::from_secs_f32(samples.len() as f32 / SAMPLE_RATE as f32);
        let _ = std::thread::Builder::new().name("soundboard".into()).spawn(move || {
            let mut pos = 0;
            let source = Box::new(move |buf: &mut [f32]| {
                for s in buf.iter_mut() {
                    *s = samples.get(pos).map_or(0.0, |v| v * volume);
                    pos += 1;
                }
            });
            match crate::voice::devices::start_output_source(device.as_deref(), source) {
                Ok(_stream) => std::thread::sleep(length + Duration::from_millis(250)),
                Err(e) => eprintln!("couldn't play sound: {e}"),
            }
        });
    }

    /// Under Camera and Screen: shows or hides the grid of sounds.
    pub(crate) fn soundboard_button(&self) -> Element<'_, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let open = self.soundboard.open;
        let (fg, fill) =
            if open { (iced::Color::WHITE, t.accent.0) } else { (t.header.0, style::darken(t.user_panel.0, -0.08)) };
        button(
            row![
                icons::icon(&icons::SOUNDBOARD, fs * 1.05, fg),
                text("Soundboard").size(fs * 0.8).color(fg).width(Length::Fill),
                icons::icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT }, fs * 0.8, fg),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding([6, 8])
        .on_press(Message::Soundboard(SoundMsg::Toggle))
        .style(style::solid_button(fill, 6.0))
        .into()
    }

    /// The grid of sounds under the call panel's buttons.
    pub(crate) fn soundboard_grid<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut grid = row![].spacing(4);
        for x in &s.sounds {
            let glyph = x.emoji.clone().unwrap_or_else(|| "🔊".into());
            let tile = button(
                column![
                    style::emoji_text(glyph, fs * 1.3),
                    text(crate::view::truncate(&x.name, 9)).size(fs * 0.7).color(t.text.0),
                ]
                .spacing(2)
                .width(Length::Fill)
                .align_x(Alignment::Center),
            )
            .width(66)
            .padding([5, 2])
            .on_press(Message::Soundboard(SoundMsg::Play(x.id)))
            .style(style::solid_button(style::darken(t.user_panel.0, 0.12), t.radius * 0.6));
            grid = grid.push(tooltip(
                tile,
                container(text(x.name.clone()).size(fs * 0.75)).padding([3, 6]).style(card(t.floating.0, 4.0)),
                tooltip::Position::Top,
            ));
        }
        container(
            iced::widget::scrollable(grid.wrap().vertical_spacing(4)).height(Length::Shrink).style(style::scroll(t)),
        )
        .max_height(200)
        .into()
    }
}

// ---- Decoding

/// Decode a sound file to 48 kHz mono samples.
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if let Some(packets) = ogg_opus_packets(bytes) {
        return decode_opus(&packets);
    }
    let (samples, rate) = decode_other(bytes)?;
    Ok(resample(&samples, rate, SAMPLE_RATE))
}

/// The packets of an Ogg file's first stream, if it's Opus (its first packet is "OpusHead").
fn ogg_opus_packets(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut at = 0;
    let mut serial = None;
    while bytes.len() >= at + 27 && &bytes[at..at + 4] == b"OggS" {
        let page_serial = u32::from_le_bytes(bytes[at + 14..at + 18].try_into().ok()?);
        let segments = bytes[at + 26] as usize;
        let table = bytes.get(at + 27..at + 27 + segments)?;
        let mut data = at + 27 + segments;
        let page_end = data + table.iter().map(|&l| l as usize).sum::<usize>();
        if page_end > bytes.len() {
            break; // cut short: keep what's complete
        }
        if *serial.get_or_insert(page_serial) == page_serial {
            for &len in table {
                partial.extend_from_slice(&bytes[data..data + len as usize]);
                data += len as usize;
                // A packet ends with a segment shorter than 255 bytes.
                if len < 255 {
                    packets.push(std::mem::take(&mut partial));
                }
            }
        }
        at = page_end;
    }
    packets.first().filter(|p| p.starts_with(b"OpusHead")).is_some().then_some(packets)
}

fn decode_opus(packets: &[Vec<u8>]) -> Result<Vec<f32>, String> {
    let head = &packets[0];
    // Samples at the start that are only there to prime the decoder.
    let pre_skip = head.get(10..12).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut frame = vec![0.0f32; 5760];
    // After the header come the tags, then the audio.
    for p in packets.iter().skip(2).filter(|p| !p.is_empty()) {
        match decoder.decode_float(p, &mut frame, false) {
            Ok(n) => out.extend_from_slice(&frame[..n]),
            Err(e) => return Err(format!("this Opus sound can't be read: {e}")),
        }
    }
    out.drain(..pre_skip.min(out.len()));
    Ok(out)
}

/// Ogg Vorbis, WAV and MP3: mono samples and their sample rate.
fn decode_other(bytes: &[u8]) -> Result<(Vec<f32>, u32), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&Hint::new(), source, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|_| "this isn't a sound Celerity can play (use Ogg, WAV or MP3)".to_string())?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("the file has no sound in it")?;
    let (track_id, mut rate) = (track.id, track.codec_params.sample_rate.unwrap_or(SAMPLE_RATE));
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("this kind of sound can't be played: {e}"))?;
    let mut out = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet: skip it, like players do.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.to_string()),
        };
        let spec = *decoded.spec();
        rate = spec.rate;
        let channels = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        out.extend(buf.samples().chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
    }
    if out.is_empty() {
        return Err("the sound is empty".into());
    }
    Ok((out, rate))
}

/// Change the sample rate (straight lines between samples: plenty for short effects).
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 {
        return samples.to_vec();
    }
    let step = from as f64 / to as f64;
    let n = (samples.len() as f64 / step).floor() as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let j = x as usize;
            let frac = (x - j as f64) as f32;
            let a = samples[j];
            let b = samples.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono 16-bit WAV of a 440 Hz tone.
    fn wav(rate: u32, ms: u32) -> Vec<u8> {
        let n = rate * ms / 1000;
        let mut data = Vec::new();
        for i in 0..n {
            let v = ((i as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 16000.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&[1, 0, 1, 0]);
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&[2, 0, 16, 0]);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// One Ogg page holding `packets` (the checksum isn't checked when reading).
    fn ogg_page(packets: &[Vec<u8>]) -> Vec<u8> {
        let mut table = Vec::new();
        let mut body = Vec::new();
        for p in packets {
            let mut left = p.len();
            while left >= 255 {
                table.push(255u8);
                left -= 255;
            }
            table.push(left as u8);
            body.extend_from_slice(p);
        }
        let mut out = b"OggS".to_vec();
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&0i64.to_le_bytes());
        out.extend_from_slice(&7u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(table.len() as u8);
        out.extend_from_slice(&table);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn decodes_wav_to_48_khz() {
        let samples = decode(&wav(8_000, 500)).unwrap();
        // Half a second at 48 kHz, give or take a sample.
        assert!((samples.len() as i64 - 24_000).abs() <= 6, "{}", samples.len());
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.4..0.55).contains(&peak), "peak {peak}");
    }

    #[test]
    fn decodes_ogg_opus() {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio).unwrap();
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 1]);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut packets = vec![head, b"OpusTags\0\0\0\0\0\0\0\0".to_vec()];
        let frame: Vec<f32> =
            (0..960).map(|i| (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5).collect();
        for _ in 0..25 {
            packets.push(encoder.encode_vec_float(&frame, 4000).unwrap());
        }
        // Headers on their own pages, the audio split over two.
        let mut file = ogg_page(&packets[..1]);
        file.extend(ogg_page(&packets[1..2]));
        file.extend(ogg_page(&packets[2..12]));
        file.extend(ogg_page(&packets[12..]));
        let samples = decode(&file).unwrap();
        assert_eq!(samples.len(), 25 * 960 - 312);
        let loud = samples[10_000..12_000].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(loud > 0.3, "it's the tone, not silence: {loud}");
        // Not Ogg Opus, and not a sound at all.
        assert!(ogg_opus_packets(&wav(8_000, 10)).is_none());
        assert!(decode(b"definitely not audio").is_err());
    }

    #[test]
    fn resampling() {
        let up = resample(&[0.0, 1.0], 24_000, 48_000);
        assert_eq!(up, vec![0.0, 0.5, 1.0, 1.0]);
        assert_eq!(resample(&[1.0; 48], 48_000, 16_000).len(), 16);
        assert_eq!(resample(&[0.25], 48_000, 48_000), vec![0.25]);
    }
}
