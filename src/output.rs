//! Output drivers. Each runs on its own thread, owns the mixer and pulls audio from it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::audio::CHANNELS;
use crate::config::OutputConfig;
use crate::engine::Mixer;

/// Time from the mixer rendering a sample to it being heard: the device buffer plus the
/// device's own latency (large for Bluetooth headphones), as reported by the audio driver.
static OUTPUT_LATENCY_NS: AtomicU64 = AtomicU64::new(0);

/// Current output latency in seconds; scheduled inputs (AirPlay) start that much earlier.
pub fn latency() -> f64 {
    OUTPUT_LATENCY_NS.load(Ordering::Relaxed) as f64 / 1e9
}

/// Start the configured output. `make_mixer` receives the output sample rate.
/// Returns that rate once the output is running.
pub fn spawn(
    cfg: &OutputConfig,
    make_mixer: impl FnOnce(u32) -> Mixer + Send + 'static,
) -> Result<u32> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
    match cfg {
        OutputConfig::Device { name } => {
            let name = name.clone();
            std::thread::Builder::new()
                .name("output-device".into())
                .spawn(move || {
                    // The cpal stream is not Send, so it lives and dies on this thread.
                    match open_device(name.as_deref(), make_mixer) {
                        Ok((stream, rate)) => {
                            let _ = ready_tx.send(Ok(rate));
                            let _keep_alive = stream;
                            loop {
                                std::thread::park();
                            }
                        }
                        Err(err) => {
                            let _ = ready_tx.send(Err(err));
                        }
                    }
                })?;
        }
        OutputConfig::Null { sample_rate } => {
            let rate = *sample_rate;
            let mut mixer = make_mixer(rate);
            std::thread::Builder::new()
                .name("output-null".into())
                .spawn(move || {
                    let _ = ready_tx.send(Ok(rate));
                    run_clocked(rate, |block| mixer.render(block));
                })?;
        }
        OutputConfig::Wav { path, sample_rate } => {
            let rate = *sample_rate;
            let spec = hound::WavSpec {
                channels: CHANNELS as u16,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(path, spec)
                .with_context(|| format!("creating {}", path.display()))?;
            let mut mixer = make_mixer(rate);
            std::thread::Builder::new()
                .name("output-wav".into())
                .spawn(move || {
                    let _ = ready_tx.send(Ok(rate));
                    let mut since_flush = 0usize;
                    run_clocked(rate, |block| {
                        mixer.render(block);
                        for &s in block.iter() {
                            let _ = writer.write_sample((s * i16::MAX as f32) as i16);
                        }
                        // Keep the header valid so the file can be opened while still recording.
                        since_flush += block.len() / CHANNELS;
                        if since_flush >= rate as usize {
                            since_flush = 0;
                            let _ = writer.flush();
                        }
                    });
                })?;
        }
    }
    ready_rx
        .recv()
        .map_err(|_| anyhow!("output thread exited during startup"))?
}

/// Pull 10 ms blocks at real-time pace.
fn run_clocked(rate: u32, mut render: impl FnMut(&mut [f32])) {
    let frames = (rate / 100) as usize;
    let mut block = vec![0.0f32; frames * CHANNELS];
    let period = Duration::from_millis(10);
    let mut next = Instant::now();
    loop {
        render(&mut block);
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

fn open_device(
    name: Option<&str>,
    make_mixer: impl FnOnce(u32) -> Mixer,
) -> Result<(cpal::Stream, u32)> {
    let host = cpal::default_host();
    let device = match name {
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?,
        Some(wanted) => {
            let wanted = wanted.to_lowercase();
            host.output_devices()?
                .find(|d| {
                    d.description()
                        .map(|desc| desc.name().to_lowercase().contains(&wanted))
                        .unwrap_or(false)
                })
                .ok_or_else(|| anyhow!("no output device matching {wanted:?}"))?
        }
    };
    let device_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate as u32;
    tracing::info!(device = %device_name, rate, channels = config.channels, ?format, "audio output");

    let mixer = make_mixer(rate);
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, mixer)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, mixer)?,
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, mixer)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, mixer)?,
        other => return Err(anyhow!("unsupported device sample format {other}")),
    };
    stream.play()?;
    Ok((stream, rate))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mixer: Mixer,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let device_channels = config.channels as usize;
    let mut stereo: Vec<f32> = vec![0.0; 4096 * CHANNELS];
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let timestamp = info.timestamp();
            let latency = timestamp.playback.duration_since(timestamp.callback);
            OUTPUT_LATENCY_NS.store(latency.as_nanos() as u64, Ordering::Relaxed);
            let frames = data.len() / device_channels;
            if stereo.len() < frames * CHANNELS {
                stereo.resize(frames * CHANNELS, 0.0);
            }
            let stereo = &mut stereo[..frames * CHANNELS];
            mixer.render(stereo);
            for (out, frame) in data
                .chunks_exact_mut(device_channels)
                .zip(stereo.as_chunks::<CHANNELS>().0)
            {
                if device_channels == 1 {
                    out[0] = T::from_sample((frame[0] + frame[1]) * 0.5);
                    continue;
                }
                out[0] = T::from_sample(frame[0]);
                out[1] = T::from_sample(frame[1]);
                for extra in &mut out[2..] {
                    *extra = T::from_sample(0.0f32);
                }
            }
        },
        |err| tracing::warn!("audio stream: {err}"),
        None,
    )?;
    Ok(stream)
}
