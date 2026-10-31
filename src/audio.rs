//! Offline audio helpers: decoding, resampling, normalization and generated sounds.
//!
//! Everything in the engine is interleaved stereo `f32` at the output sample rate.

use std::io::Cursor;

use anyhow::{Context, Result, anyhow};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub const CHANNELS: usize = 2;

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Decode any supported container/codec (WAV, AIFF, MP3, AAC/M4A, FLAC, Ogg Vorbis, ALAC)
/// into interleaved stereo at `target_rate`.
pub fn decode(bytes: Vec<u8>, extension: Option<&str>, target_rate: u32) -> Result<Vec<f32>> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = extension {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unrecognized audio format")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("missing audio codec parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .context("unsupported codec")?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut source_rate = 0u32;
    let mut source_channels = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(err).context("reading audio packet"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(_) | SymphoniaError::IoError(_)) => continue,
            Err(err) => return Err(err).context("decoding audio"),
        };
        source_rate = buf.spec().rate();
        source_channels = buf.spec().channels().count();
        let start = interleaved.len();
        interleaved.resize(start + buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut interleaved[start..]);
    }
    if interleaved.is_empty() || source_channels == 0 {
        return Err(anyhow!("audio contained no samples"));
    }

    let stereo = to_stereo(&interleaved, source_channels);
    resample(stereo, source_rate, target_rate)
}

fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        2 => samples.to_vec(),
        n => samples.chunks_exact(n).flat_map(|f| [f[0], f[1]]).collect(),
    }
}

pub fn resample(stereo: Vec<f32>, from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to {
        return Ok(stereo);
    }
    let frames = stereo.len() / CHANNELS;
    let input = InterleavedSlice::new(&stereo[..], CHANNELS, frames)?;
    let mut resampler =
        Fft::<f32>::new(from as usize, to as usize, 1024, CHANNELS, FixedSync::Both)?;
    let output = resampler.process_all(&input, frames, None)?;
    Ok(output.take_data())
}

/// Scale to a target RMS level, without letting peaks exceed -1 dBFS.
pub fn normalize(samples: &mut [f32], target_dbfs: f32) {
    if samples.is_empty() {
        return;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    let peak = samples.iter().fold(0f32, |p, s| p.max(s.abs()));
    if rms < 1e-6 {
        return;
    }
    let gain = (db_to_gain(target_dbfs) / rms).min(db_to_gain(-1.0) / peak);
    samples.iter_mut().for_each(|s| *s *= gain);
}

pub fn silence(rate: u32, ms: u32) -> Vec<f32> {
    vec![0.0; (rate as usize * ms as usize / 1000) * CHANNELS]
}

/// A soft two-tone "ding-dong".
pub fn notification_chime(rate: u32) -> Vec<f32> {
    let mut out = tone(rate, 880.0, 380, 0.35);
    out.extend(tone(rate, 659.25, 520, 0.35));
    out
}

/// Two bursts of three short high beeps.
pub fn critical_chime(rate: u32) -> Vec<f32> {
    let mut out = Vec::new();
    for burst in 0..2 {
        for _ in 0..3 {
            out.extend(tone(rate, 1046.5, 110, 0.45));
            out.extend(silence(rate, 60));
        }
        if burst == 0 {
            out.extend(silence(rate, 250));
        }
    }
    out
}

/// A sine tone with a short attack and an exponential decay, as interleaved stereo.
fn tone(rate: u32, freq: f32, ms: u32, amplitude: f32) -> Vec<f32> {
    let frames = rate as usize * ms as usize / 1000;
    let attack = (rate as usize / 200).max(1);
    let decay_rate = 5.0 / frames as f32;
    (0..frames)
        .flat_map(|i| {
            let t = i as f32 / rate as f32;
            let env = (i as f32 / attack as f32).min(1.0) * (-(i as f32) * decay_rate).exp();
            let s = (t * freq * std::f32::consts::TAU).sin() * amplitude * env;
            [s, s]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_hits_target_without_clipping() {
        let mut samples: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.01)
            .collect();
        normalize(&mut samples, -18.0);
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!((20.0 * rms.log10() + 18.0).abs() < 0.1);
        assert!(samples.iter().all(|s| s.abs() <= db_to_gain(-1.0) + 1e-6));
    }

    #[test]
    fn resample_changes_length_by_ratio() {
        let input = vec![0.1f32; 44_100 * CHANNELS];
        let out = resample(input, 44_100, 48_000).unwrap();
        let frames = out.len() / CHANNELS;
        assert!((frames as i64 - 48_000).abs() < 50, "got {frames} frames");
    }
}
