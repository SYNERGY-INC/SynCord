//! A short "you were mentioned" chime, made on the fly (no sound files to ship).

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

const NOTES: [(f32, f32); 2] = [(988.0, 0.09), (1319.0, 0.16)]; // B5 then E6, seconds

/// The chime as mono samples at `rate`.
pub fn chime(rate: u32, volume: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for (freq, secs) in NOTES {
        let n = (secs * rate as f32) as usize;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            // Quick attack, gentle decay, so it doesn't click.
            let env = (t / 0.004).min(1.0) * (-t * 18.0).exp();
            out.push((t * freq * std::f32::consts::TAU).sin() * env * 0.25 * volume.clamp(0.0, 1.5));
        }
    }
    out
}

/// Play the chime on the chosen speakers without blocking (does nothing if there are none).
pub fn ping(device: Option<String>, volume: f32) {
    let _ = std::thread::Builder::new().name("syncord-ping".into()).spawn(move || {
        let Some(device) = crate::voice::devices::find_output(device.as_deref()) else { return };
        let Ok(config) = device.default_output_config() else { return };
        let format = config.sample_format();
        let config: StreamConfig = config.config();
        let samples = chime(config.sample_rate, volume);
        let seconds = samples.len() as f32 / config.sample_rate as f32;

        fn build<T: SizedSample + FromSample<f32>>(
            device: &cpal::Device,
            config: StreamConfig,
            samples: Vec<f32>,
        ) -> Option<cpal::Stream> {
            let channels = config.channels as usize;
            let mut pos = 0;
            device
                .build_output_stream(
                    config,
                    move |data: &mut [T], _| {
                        for frame in data.chunks_mut(channels) {
                            let v = T::from_sample(samples.get(pos).copied().unwrap_or(0.0));
                            pos += 1;
                            frame.iter_mut().for_each(|s| *s = v);
                        }
                    },
                    |_| {},
                    None,
                )
                .ok()
        }
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, samples),
            SampleFormat::I16 => build::<i16>(&device, config, samples),
            SampleFormat::U16 => build::<u16>(&device, config, samples),
            SampleFormat::I32 => build::<i32>(&device, config, samples),
            _ => None,
        };
        if let Some(stream) = stream
            && stream.play().is_ok()
        {
            std::thread::sleep(std::time::Duration::from_secs_f32(seconds + 0.15));
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn chime_is_short_quiet_and_fades() {
        let c = super::chime(48_000, 1.0);
        assert!((0.2..0.3).contains(&(c.len() as f32 / 48_000.0)));
        assert!(c.iter().all(|s| s.abs() <= 0.26));
        assert!(c.last().unwrap().abs() < 0.02, "ends quietly");
    }
}
