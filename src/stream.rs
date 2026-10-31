//! Plumbing shared by every continuous input (files, radio, AirPlay).
//!
//! A decoder thread owns a [`StreamWriter`]: it pushes PCM at any sample rate, which is resampled
//! to the engine rate and queued in a lock-free ring buffer. The mixer owns the matching
//! [`StreamSource`], which prebuffers, handles underruns, and for live inputs nudges playback
//! speed by dropping or repeating single frames so sender/receiver clock drift never piles up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::audio::CHANNELS;
use crate::engine::Source;

/// Frames between drift corrections; one frame per 2000 is a 0.05% speed change.
const DRIFT_INTERVAL: usize = 2000;
/// Scheduled streams correct faster (0.2%): still inaudible, but keeps lip sync tight.
const SYNC_INTERVAL: usize = 500;

#[derive(Default)]
struct Shared {
    /// Set by the writer to discard everything queued; cleared by the source once done.
    flush: AtomicBool,
    /// Set when the source is dropped, so the writer's thread can stop.
    closed: AtomicBool,
    /// Whether the source produced audio recently.
    active: AtomicBool,
    gain: AtomicU32,
    /// Scheduled playback: desired queue length in samples (negative when unscheduled),
    /// measured when `rendered` was `sync_rendered_at`.
    sync_target: AtomicI64,
    sync_rendered_at: AtomicU64,
    /// Samples the mixer has asked this source for: the output clock, advancing whether or
    /// not audio was played.
    rendered: AtomicU64,
}

/// Read-only view of a stream for status reporting.
#[derive(Clone)]
pub struct StreamStatus(Arc<Shared>);

impl StreamStatus {
    pub fn active(&self) -> bool {
        self.0.active.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy)]
pub struct StreamOptions {
    /// Audio buffered before playback starts or resumes after an underrun.
    pub target_ms: u32,
    /// Ring buffer size.
    pub capacity_ms: u32,
    /// Live inputs are paced by a remote clock: correct drift around `target_ms`.
    /// Non-live inputs (files) are paced by the reader, which blocks while the buffer is full.
    pub live: bool,
}

pub fn stream(rate: u32, options: StreamOptions) -> (StreamWriter, StreamSource) {
    let samples = |ms: u32| rate as usize * ms as usize / 1000 * CHANNELS;
    let (producer, consumer) = RingBuffer::new(samples(options.capacity_ms));
    let shared = Arc::new(Shared {
        gain: AtomicU32::new(1f32.to_bits()),
        sync_target: AtomicI64::new(-1),
        ..Shared::default()
    });
    let target = samples(options.target_ms);
    let writer = StreamWriter {
        producer,
        shared: Arc::clone(&shared),
        rate,
        resampler: None,
    };
    let source = StreamSource {
        consumer,
        shared,
        target,
        margin: (target / 2).max(samples(40)),
        live: options.live,
        playing: false,
        quiet_frames: usize::MAX / 2,
        hold_frames: rate as usize / 2,
        until_adjust: DRIFT_INTERVAL,
        last: [0.0; CHANNELS],
        sync_tolerance: samples(5) as i64,
        sync_hard_limit: samples(100) as i64,
    };
    (writer, source)
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The source was removed from the mixer; the writer should stop.
    Closed,
}

pub struct StreamWriter {
    producer: Producer<f32>,
    shared: Arc<Shared>,
    rate: u32,
    resampler: Option<StreamResampler>,
}

impl StreamWriter {
    pub fn status(&self) -> StreamStatus {
        StreamStatus(Arc::clone(&self.shared))
    }

    /// The engine sample rate this writer produces.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared
            .gain
            .store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Queue interleaved audio. With `block`, wait for space (file inputs); otherwise drop what
    /// does not fit (live inputs, which must never stall their network thread).
    pub fn write(
        &mut self,
        samples: &[f32],
        channels: usize,
        rate: u32,
        block: bool,
    ) -> Result<(), WriteError> {
        if self.is_closed() {
            return Err(WriteError::Closed);
        }
        if channels == 0 || samples.is_empty() {
            return Ok(());
        }
        let stereo = to_stereo(samples, channels);
        if rate == self.rate {
            return self.push(&stereo, block);
        }
        if self.resampler.as_ref().is_none_or(|r| r.from != rate) {
            match StreamResampler::new(rate, self.rate) {
                Ok(r) => self.resampler = Some(r),
                Err(err) => {
                    tracing::warn!("cannot resample {rate} Hz: {err:#}");
                    return Ok(());
                }
            }
        }
        let mut resampler = self.resampler.take().expect("resampler set above");
        let resampled = resampler.process(&stereo);
        self.resampler = Some(resampler);
        self.push(&resampled, block)
    }

    /// Write silence, e.g. to conceal a lost packet.
    pub fn write_silence(&mut self, frames: usize, rate: u32) -> Result<(), WriteError> {
        let zeros = vec![0.0; frames * CHANNELS];
        self.write(&zeros, CHANNELS, rate, false)
    }

    fn push(&mut self, samples: &[f32], block: bool) -> Result<(), WriteError> {
        let mut rest = samples;
        while !rest.is_empty() {
            let n = self.producer.slots().min(rest.len());
            let n = n - n % CHANNELS;
            if n > 0 {
                if let Ok(mut chunk) = self.producer.write_chunk_uninit(n) {
                    let (a, b) = chunk.as_mut_slices();
                    for (dst, src) in a.iter_mut().chain(b.iter_mut()).zip(&rest[..n]) {
                        dst.write(*src);
                    }
                    // SAFETY: every slot of the chunk was initialized just above.
                    unsafe { chunk.commit_all() };
                }
                rest = &rest[n..];
                continue;
            }
            if !block {
                return Ok(()); // Buffer full: drop the overflow.
            }
            if self.is_closed() {
                return Err(WriteError::Closed);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Schedule playback: the queue should hold `samples` right now (the newest queued sample
    /// is due that far in the future). The source drops, waits or nudges to match.
    pub fn set_sync_target(&self, samples: i64) {
        let rendered = self.shared.rendered.load(Ordering::Acquire);
        self.shared
            .sync_rendered_at
            .store(rendered, Ordering::Release);
        self.shared
            .sync_target
            .store(samples.max(0), Ordering::Release);
    }

    /// Seconds of audio accepted by `write` that are still inside the resampler.
    pub fn pipeline_delay(&self) -> f64 {
        self.resampler
            .as_ref()
            .map_or(0.0, |r| r.delay_seconds(self.rate))
    }

    /// Discard everything queued but not yet played (pause, seek, track change).
    pub fn flush(&mut self) {
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
        self.shared.flush.store(true, Ordering::Release);
        // The mixer clears the flag on its next block; wait so new audio isn't flushed too.
        for _ in 0..50 {
            if !self.shared.flush.load(Ordering::Acquire) || self.is_closed() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

pub struct StreamSource {
    consumer: Consumer<f32>,
    shared: Arc<Shared>,
    target: usize,
    margin: usize,
    live: bool,
    playing: bool,
    /// Frames since real audio last played, for the `active` flag.
    quiet_frames: usize,
    hold_frames: usize,
    until_adjust: usize,
    last: [f32; CHANNELS],
    sync_tolerance: i64,
    sync_hard_limit: i64,
}

impl Drop for StreamSource {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.active.store(false, Ordering::Relaxed);
    }
}

impl StreamSource {
    fn pop_frame(&mut self) -> Option<[f32; CHANNELS]> {
        if self.consumer.slots() < CHANNELS {
            return None;
        }
        let chunk = self.consumer.read_chunk(CHANNELS).ok()?;
        let (a, b) = chunk.as_slices();
        let mut frame = [0.0; CHANNELS];
        for (dst, src) in frame.iter_mut().zip(a.iter().chain(b)) {
            *dst = *src;
        }
        chunk.commit_all();
        Some(frame)
    }

    fn discard(&mut self, samples: usize) {
        let n = samples.min(self.consumer.slots());
        let n = n - n % CHANNELS;
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            chunk.commit_all();
        }
    }

    /// For scheduled streams: queued samples minus the samples that should be queued now.
    /// Positive means late (too much queued), negative means early.
    fn sync_error(&self) -> Option<i64> {
        let target = self.shared.sync_target.load(Ordering::Acquire);
        if target < 0 {
            return None;
        }
        let at = self.shared.sync_rendered_at.load(Ordering::Acquire);
        let rendered = self.shared.rendered.load(Ordering::Acquire);
        let expected = target - rendered.saturating_sub(at) as i64;
        Some(self.consumer.slots() as i64 - expected)
    }
}

impl Source for StreamSource {
    fn render(&mut self, out: &mut [f32]) -> bool {
        if self.shared.flush.load(Ordering::Acquire) {
            self.discard(self.consumer.slots());
            self.playing = false;
            self.shared.flush.store(false, Ordering::Release);
        }

        let sync_error = self.sync_error();
        // For a scheduled stream that is waiting: frames of silence until its head is due.
        let mut start_in = None;
        match sync_error {
            Some(err) if err > self.sync_hard_limit => {
                // Far behind schedule: drop the audio that should already have played.
                self.discard(err as usize);
                self.playing = true;
            }
            Some(err) if err >= -self.sync_tolerance => self.playing |= self.consumer.slots() > 0,
            Some(err) if !self.playing || err < -self.sync_hard_limit => {
                // Early: hold silence until the head of the queue is due.
                self.playing = false;
                start_in = Some((-err) as usize / CHANNELS);
            }
            Some(_) => {}
            None if !self.playing && self.consumer.slots() >= self.target => self.playing = true,
            None => {}
        }
        let (interval, scheduled_drift) = match sync_error {
            Some(err) if err > self.sync_tolerance => (SYNC_INTERVAL, 1),
            Some(err) if err < -self.sync_tolerance => (SYNC_INTERVAL, -1),
            Some(_) => (SYNC_INTERVAL, 0),
            None => (DRIFT_INTERVAL, 0),
        };
        self.until_adjust = self.until_adjust.min(interval);

        let gain = f32::from_bits(self.shared.gain.load(Ordering::Relaxed));
        let mut produced = 0usize;
        for frame in out.as_chunks_mut::<CHANNELS>().0 {
            if !self.playing {
                match start_in {
                    Some(0) => self.playing = true,
                    Some(n) => {
                        start_in = Some(n - 1);
                        *frame = [0.0; CHANNELS];
                        continue;
                    }
                    None => {
                        *frame = [0.0; CHANNELS];
                        continue;
                    }
                }
            }
            if self.live {
                self.until_adjust -= 1;
                if self.until_adjust == 0 {
                    self.until_adjust = interval;
                    let drift = if sync_error.is_some() {
                        scheduled_drift
                    } else {
                        let queued = self.consumer.slots();
                        if queued > self.target + self.margin {
                            1
                        } else if queued + self.margin < self.target {
                            -1
                        } else {
                            0
                        }
                    };
                    if drift > 0 {
                        let _ = self.pop_frame(); // Running behind: skip a frame.
                    } else if drift < 0 {
                        *frame = self.last.map(|s| s * gain); // Running ahead: repeat one.
                        continue;
                    }
                }
            }
            match self.pop_frame() {
                Some(f) => {
                    self.last = f;
                    *frame = f.map(|s| s * gain);
                    produced += 1;
                }
                None => {
                    // Underrun: go silent and rebuffer.
                    self.playing = false;
                    *frame = [0.0; CHANNELS];
                }
            }
        }

        self.shared
            .rendered
            .fetch_add(out.len() as u64, Ordering::Release);
        let frames = out.len() / CHANNELS;
        self.quiet_frames = if produced > 0 {
            0
        } else {
            self.quiet_frames.saturating_add(frames)
        };
        let active = self.quiet_frames < self.hold_frames;
        self.shared.active.store(active, Ordering::Relaxed);
        active
    }
}

fn to_stereo(samples: &[f32], channels: usize) -> std::borrow::Cow<'_, [f32]> {
    match channels {
        2 => std::borrow::Cow::Borrowed(samples),
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        n => samples.chunks_exact(n).flat_map(|f| [f[0], f[1]]).collect(),
    }
}

/// Chunked stereo resampler for streams: accepts any number of frames per call.
pub struct StreamResampler {
    from: u32,
    inner: Fft<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> Result<Self> {
        let inner = Fft::<f32>::new(from as usize, to as usize, 1024, CHANNELS, FixedSync::Input)?;
        let output = vec![0.0; inner.output_frames_max() * CHANNELS];
        Ok(Self {
            from,
            inner,
            pending: Vec::new(),
            output,
        })
    }

    pub fn process(&mut self, stereo: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(stereo);
        let mut result = Vec::new();
        loop {
            let needed = self.inner.input_frames_next();
            if self.pending.len() < needed * CHANNELS {
                break;
            }
            let input = InterleavedSlice::new(&self.pending[..needed * CHANNELS], CHANNELS, needed);
            let frames_out = self.output.len() / CHANNELS;
            let output = InterleavedSlice::new_mut(&mut self.output[..], CHANNELS, frames_out);
            let (Ok(input), Ok(mut output)) = (input, output) else {
                break;
            };
            match self.inner.process_into_buffer(&input, &mut output, None) {
                Ok((used, written)) => {
                    result.extend_from_slice(&self.output[..written * CHANNELS]);
                    self.pending.drain(..used * CHANNELS);
                }
                Err(err) => {
                    tracing::warn!("resampler: {err}");
                    self.pending.clear();
                    break;
                }
            }
        }
        result
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        self.pending.clear();
    }

    /// Time between an input frame being accepted and its resampled output being produced.
    fn delay_seconds(&self, to: u32) -> f64 {
        let pending = self.pending.len() / CHANNELS;
        pending as f64 / self.from as f64 + self.inner.output_delay() as f64 / to as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: StreamOptions = StreamOptions {
        target_ms: 100,
        capacity_ms: 1000,
        live: true,
    };

    #[test]
    fn prebuffers_then_plays_and_underruns() {
        let (mut writer, mut source) = stream(1000, OPTS);
        let mut out = vec![0.0; 20 * CHANNELS];

        writer
            .write(&vec![0.5; 50 * CHANNELS], CHANNELS, 1000, false)
            .unwrap();
        source.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "waits for the target level");

        writer
            .write(&vec![0.5; 60 * CHANNELS], CHANNELS, 1000, false)
            .unwrap();
        assert!(source.render(&mut out));
        assert!(out.iter().all(|&s| s == 0.5));

        let mut long = vec![0.0; 200 * CHANNELS];
        source.render(&mut long);
        assert_eq!(long[89 * CHANNELS], 0.5);
        assert_eq!(long[91 * CHANNELS], 0.0, "underrun goes silent");
    }

    #[test]
    fn resamples_mono_input() {
        let (mut writer, mut source) = stream(48_000, OPTS);
        writer.write(&vec![0.25; 44_100], 1, 44_100, false).unwrap();
        let mut out = vec![0.0; 10_000 * CHANNELS];
        source.render(&mut out);
        let mid = out[5_000 * CHANNELS];
        assert!((mid - 0.25).abs() < 0.01, "got {mid}");
    }

    #[test]
    fn scheduled_playback_waits_until_due_then_tracks_the_schedule() {
        let (mut writer, mut source) = stream(1000, OPTS);
        let ramp: Vec<f32> = (1..=400).flat_map(|i| [i as f32, i as f32]).collect();
        writer.write(&ramp, CHANNELS, 1000, false).unwrap();
        // The newest of the 400 queued frames is due in 600 ms, so the first is due in 200 ms.
        writer.set_sync_target(600 * CHANNELS as i64);

        let mut out = vec![0.0; 150 * CHANNELS];
        source.render(&mut out);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "waits while the head is not yet due"
        );

        let mut out = vec![0.0; 100 * CHANNELS];
        source.render(&mut out);
        let first = out
            .as_chunks::<2>()
            .0
            .iter()
            .position(|f| f[0] != 0.0 || f[1] != 0.0);
        assert_eq!(first, Some(50), "starts exactly when the head is due");
        assert_eq!(
            out[50 * CHANNELS],
            1.0,
            "starting with the first queued frame"
        );

        // Late by 300 ms (more than the hard limit): the overdue audio is dropped.
        let (mut writer, mut source) = stream(1000, OPTS);
        writer.write(&ramp, CHANNELS, 1000, false).unwrap();
        writer.set_sync_target(100 * CHANNELS as i64);
        let mut out = vec![0.0; 10 * CHANNELS];
        source.render(&mut out);
        assert!(
            out[0] >= 299.0,
            "skipped to the frame that is due now, got {}",
            out[0]
        );
    }

    #[test]
    fn closed_after_source_dropped() {
        let (mut writer, source) = stream(1000, OPTS);
        drop(source);
        assert_eq!(
            writer.write(&[0.0; 2], 2, 1000, false),
            Err(WriteError::Closed)
        );
    }
}
