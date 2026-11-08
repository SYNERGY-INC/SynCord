//! Microphone and speaker access through cpal.

use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig};
use syncord_proto::voice::SAMPLE_RATE;

use super::dsp::Resampler;
use super::mixer::Mixer;

fn device_name(d: &Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

pub fn input_names() -> Vec<String> {
    cpal::default_host().input_devices().map(|it| it.filter_map(|d| device_name(&d)).collect()).unwrap_or_default()
}

pub fn output_names() -> Vec<String> {
    cpal::default_host().output_devices().map(|it| it.filter_map(|d| device_name(&d)).collect()).unwrap_or_default()
}

fn find_input(name: Option<&str>) -> Option<Device> {
    let host = cpal::default_host();
    name.and_then(|n| host.input_devices().ok()?.find(|d| device_name(d).as_deref() == Some(n)))
        .or_else(|| host.default_input_device())
}

fn find_output(name: Option<&str>) -> Option<Device> {
    let host = cpal::default_host();
    name.and_then(|n| host.output_devices().ok()?.find(|d| device_name(d).as_deref() == Some(n)))
        .or_else(|| host.default_output_device())
}

/// Use 48 kHz if the device supports it (no resampling needed), else its default.
fn pick_config(
    default: SupportedStreamConfig,
    supported: Vec<cpal::SupportedStreamConfigRange>,
) -> SupportedStreamConfig {
    supported
        .into_iter()
        .filter(|r| r.channels() == default.channels() && r.sample_format() == default.sample_format())
        .find(|r| r.min_sample_rate() <= SAMPLE_RATE && r.max_sample_rate() >= SAMPLE_RATE)
        .map(|r| r.with_sample_rate(SAMPLE_RATE))
        .unwrap_or(default)
}

/// Start capturing. Mono 48 kHz chunks are sent to `tx`.
pub fn start_input(name: Option<&str>, tx: SyncSender<Vec<f32>>) -> Result<Stream, String> {
    let device = find_input(name).ok_or("No microphone found")?;
    let default = device.default_input_config().map_err(|e| format!("Microphone unavailable: {e}"))?;
    let supported = device.supported_input_configs().map(|c| c.collect()).unwrap_or_default();
    let config = pick_config(default, supported);
    let format = config.sample_format();
    let config: StreamConfig = config.config();
    eprintln!(
        "microphone: {} ({} Hz, {} ch, {format:?})",
        device_name(&device).unwrap_or_default(),
        config.sample_rate,
        config.channels
    );

    fn build<T: SizedSample>(device: &Device, config: StreamConfig, tx: SyncSender<Vec<f32>>) -> Result<Stream, String>
    where
        f32: FromSample<T>,
    {
        let channels = config.channels as usize;
        let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
        let mut mono = Vec::new();
        device
            .build_input_stream(
                config,
                move |data: &[T], _| {
                    mono.clear();
                    mono.extend(
                        data.chunks(channels)
                            .map(|frame| frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32)
                            // A misbehaving driver must not poison the encoder with NaN/inf.
                            .map(|x| if x.is_finite() { x } else { 0.0 }),
                    );
                    let mut out = Vec::with_capacity(mono.len() + 64);
                    resampler.process(&mono, &mut out);
                    // If the worker falls behind, dropping audio beats growing latency.
                    let _ = tx.try_send(out);
                },
                |e| eprintln!("microphone error: {e}"),
                None,
            )
            .map_err(|e| format!("Couldn't open microphone: {e}"))
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, tx),
        SampleFormat::I16 => build::<i16>(&device, config, tx),
        SampleFormat::U16 => build::<u16>(&device, config, tx),
        SampleFormat::I32 => build::<i32>(&device, config, tx),
        SampleFormat::U8 => build::<u8>(&device, config, tx),
        other => Err(format!("Unsupported microphone format {other:?}")),
    }?;
    stream.play().map_err(|e| format!("Couldn't start microphone: {e}"))?;
    Ok(stream)
}

/// Pulls 48 kHz audio from the mixer and converts it to the device's rate.
struct OutputStage {
    mixer: Arc<Mixer>,
    step: f64,
    t: f64,
    a: f32,
    b: f32,
    chunk: Vec<f32>,
    pos: usize,
}

impl OutputStage {
    fn next_source(&mut self) -> f32 {
        if self.pos >= self.chunk.len() {
            self.mixer.fill(&mut self.chunk);
            self.pos = 0;
        }
        self.pos += 1;
        self.chunk[self.pos - 1]
    }

    fn next(&mut self) -> f32 {
        if self.step == 1.0 {
            return self.next_source();
        }
        while self.t >= 1.0 {
            self.t -= 1.0;
            self.a = self.b;
            self.b = self.next_source();
        }
        let y = self.a + (self.b - self.a) * self.t as f32;
        self.t += self.step;
        y
    }
}

pub fn start_output(name: Option<&str>, mixer: Arc<Mixer>) -> Result<Stream, String> {
    let device = find_output(name).ok_or("No speakers or headphones found")?;
    let default = device.default_output_config().map_err(|e| format!("Speakers unavailable: {e}"))?;
    let supported = device.supported_output_configs().map(|c| c.collect()).unwrap_or_default();
    let config = pick_config(default, supported);
    let format = config.sample_format();
    let config: StreamConfig = config.config();
    eprintln!(
        "speakers: {} ({} Hz, {} ch, {format:?})",
        device_name(&device).unwrap_or_default(),
        config.sample_rate,
        config.channels
    );

    let stage = OutputStage {
        mixer,
        step: SAMPLE_RATE as f64 / config.sample_rate as f64,
        t: 1.0,
        a: 0.0,
        b: 0.0,
        chunk: vec![0.0; 480],
        pos: 480,
    };

    fn build<T: SizedSample + FromSample<f32>>(
        device: &Device,
        config: StreamConfig,
        mut stage: OutputStage,
    ) -> Result<Stream, String> {
        let channels = config.channels as usize;
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _| {
                    for frame in data.chunks_mut(channels) {
                        let v = T::from_sample(stage.next());
                        frame.iter_mut().for_each(|s| *s = v);
                    }
                },
                |e| eprintln!("speaker error: {e}"),
                None,
            )
            .map_err(|e| format!("Couldn't open speakers: {e}"))
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, stage),
        SampleFormat::I16 => build::<i16>(&device, config, stage),
        SampleFormat::U16 => build::<u16>(&device, config, stage),
        SampleFormat::I32 => build::<i32>(&device, config, stage),
        SampleFormat::U8 => build::<u8>(&device, config, stage),
        other => Err(format!("Unsupported speaker format {other:?}")),
    }?;
    stream.play().map_err(|e| format!("Couldn't start speakers: {e}"))?;
    Ok(stream)
}

// Keep the Sample trait in scope for `from_sample` on all platforms.
#[allow(unused_imports)]
use Sample as _;
