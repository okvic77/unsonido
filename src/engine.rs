//! The real-time mixer. It runs inside the audio callback, so nothing here may block, and it
//! avoids allocating: control happens through bounded channels and finished clips are sent
//! back to the control side to be dropped there.
//!
//! Layers, lowest to highest priority:
//!   background (radio, playlists) -> foreground (AirPlay, later) -> notification -> critical
//! The foreground crossfades the background out; alerts duck everything below them.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::audio::{CHANNELS, db_to_gain};
use crate::config::MixerConfig;

/// Largest block the mixer renders at once; callbacks larger than this are split.
const MAX_BLOCK: usize = 1024;
const QUEUE_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertPriority {
    Notification,
    Critical,
}

/// An alert rendered to interleaved stereo at the engine rate, ready to play.
pub struct PreparedAlert {
    pub id: String,
    pub priority: AlertPriority,
    pub audio: Arc<Vec<f32>>,
}

pub enum Command {
    Enqueue(PreparedAlert),
    /// Stop the alert that is playing now.
    Skip,
    /// Stop the current alert and drop everything queued.
    Clear,
    SetVolume(f32),
    /// Add an input, replacing any existing input with the same name.
    AddSource {
        name: String,
        layer: Layer,
        source: Box<dyn Source>,
    },
    RemoveSource(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishReason {
    Completed,
    Skipped,
    Cleared,
}

pub enum Event {
    Started {
        id: String,
    },
    /// A notification was interrupted by a critical alert and put back at the head of the queue.
    Preempted {
        id: String,
    },
    Finished {
        alert: PreparedAlert,
        reason: FinishReason,
    },
    /// A removed or replaced input, handed back so it is dropped off the audio thread.
    SourceRemoved(Box<dyn Source>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Background,
    Foreground,
}

/// A continuous input: a looping file now, AirPlay or an Icecast stream later.
pub trait Source: Send {
    /// Fill `out` with interleaved stereo. Return `false` when the source had nothing to play,
    /// which lets lower layers come back up.
    fn render(&mut self, out: &mut [f32]) -> bool;
}

/// Loops an in-memory clip forever.
#[cfg(test)]
pub struct LoopSource {
    pcm: Vec<f32>,
    pos: usize,
    gain: f32,
}

#[cfg(test)]
impl LoopSource {
    pub fn new(pcm: Vec<f32>, gain: f32) -> Self {
        Self { pcm, pos: 0, gain }
    }
}

#[cfg(test)]
impl Source for LoopSource {
    fn render(&mut self, out: &mut [f32]) -> bool {
        if self.pcm.is_empty() {
            return false;
        }
        for sample in out.iter_mut() {
            *sample = self.pcm[self.pos] * self.gain;
            self.pos = (self.pos + 1) % self.pcm.len();
        }
        true
    }
}

/// Mixer state shared with the control side for status reporting.
#[derive(Default)]
pub struct Levels {
    duck: AtomicU32,
    master: AtomicU32,
    foreground_active: AtomicBool,
}

impl Levels {
    pub fn duck(&self) -> f32 {
        f32::from_bits(self.duck.load(Ordering::Relaxed))
    }

    pub fn master(&self) -> f32 {
        f32::from_bits(self.master.load(Ordering::Relaxed))
    }

    pub fn foreground_active(&self) -> bool {
        self.foreground_active.load(Ordering::Relaxed)
    }
}

/// A gain that moves linearly toward its target at separate up and down speeds.
struct Ramp {
    value: f32,
    target: f32,
    up_step: f32,
    down_step: f32,
}

impl Ramp {
    fn new(value: f32, rate: u32, up_ms: u32, down_ms: u32) -> Self {
        let step = |ms: u32| 1.0 / (rate as f32 * ms.max(1) as f32 / 1000.0);
        Self {
            value,
            target: value,
            up_step: step(up_ms),
            down_step: step(down_ms),
        }
    }

    fn next(&mut self) -> f32 {
        if self.value < self.target {
            self.value = (self.value + self.up_step).min(self.target);
        } else if self.value > self.target {
            self.value = (self.value - self.down_step).max(self.target);
        }
        self.value
    }

    fn settled(&self) -> bool {
        (self.value - self.target).abs() < 1e-3
    }
}

struct LayerSlot {
    name: String,
    layer: Layer,
    source: Box<dyn Source>,
    gain: Ramp,
    active: bool,
}

struct Playing {
    alert: PreparedAlert,
    pos: usize,
    /// Frames of silence before the clip starts, so the duck finishes ramping first.
    preroll: usize,
}

pub struct Mixer {
    commands: Receiver<Command>,
    events: Sender<Event>,
    levels: Arc<Levels>,
    layers: Vec<LayerSlot>,
    queue: VecDeque<PreparedAlert>,
    current: Option<Playing>,
    /// Priority currently holding the bed down; cleared after the release delay.
    ducking: Option<AlertPriority>,
    tail_left: usize,
    duck: Ramp,
    master: Ramp,
    master_volume: f32,
    notification_gain: f32,
    critical_gain: f32,
    critical_min_volume: f32,
    attack_frames: usize,
    release_delay_frames: usize,
    rate: u32,
    crossfade_ms: u32,
    bed: Vec<f32>,
    scratch: Vec<f32>,
    /// Copies of the final mix for pipe outputs.
    taps: Vec<rtrb::Producer<f32>>,
}

impl Mixer {
    pub fn new(
        rate: u32,
        cfg: &MixerConfig,
        commands: Receiver<Command>,
        events: Sender<Event>,
        levels: Arc<Levels>,
    ) -> Self {
        let frames = |ms: u32| rate as usize * ms as usize / 1000;
        let master_volume = cfg.master_volume.clamp(0.0, 1.0);
        Self {
            commands,
            events,
            levels,
            layers: Vec::with_capacity(32),
            queue: VecDeque::with_capacity(QUEUE_CAPACITY),
            current: None,
            ducking: None,
            tail_left: 0,
            duck: Ramp::new(1.0, rate, cfg.release_ms, cfg.attack_ms),
            master: Ramp::new(master_volume, rate, 150, 150),
            master_volume,
            notification_gain: db_to_gain(cfg.notification_duck_db),
            critical_gain: db_to_gain(cfg.critical_duck_db),
            critical_min_volume: cfg.critical_min_volume.clamp(0.0, 1.0),
            attack_frames: frames(cfg.attack_ms),
            release_delay_frames: frames(cfg.release_delay_ms),
            rate,
            crossfade_ms: cfg.crossfade_ms,
            bed: vec![0.0; MAX_BLOCK * CHANNELS],
            scratch: vec![0.0; MAX_BLOCK * CHANNELS],
            taps: Vec::new(),
        }
    }

    pub fn add_source(&mut self, name: String, layer: Layer, source: Box<dyn Source>) {
        self.remove_source(&name);
        let gain = Ramp::new(0.0, self.rate, self.crossfade_ms, self.crossfade_ms);
        self.layers.push(LayerSlot {
            name,
            layer,
            source,
            gain,
            active: false,
        });
    }

    fn remove_source(&mut self, name: &str) {
        if let Some(pos) = self.layers.iter().position(|s| s.name == name) {
            let slot = self.layers.swap_remove(pos);
            self.emit(Event::SourceRemoved(slot.source));
        }
    }

    pub fn add_tap(&mut self, tap: rtrb::Producer<f32>) {
        self.taps.push(tap);
    }

    /// Render interleaved stereo into `out`.
    pub fn render(&mut self, out: &mut [f32]) {
        for chunk in out.chunks_mut(MAX_BLOCK * CHANNELS) {
            self.render_block(chunk);
        }
        for tap in &mut self.taps {
            // If a pipe output falls behind, it loses audio rather than stalling the mix.
            if let Ok(chunk) = tap.write_chunk_uninit(out.len()) {
                chunk.fill_from_iter(out.iter().copied());
            }
        }
    }

    fn render_block(&mut self, out: &mut [f32]) {
        self.handle_commands();
        self.advance_queue();
        self.update_targets();

        let len = out.len();
        let bed = &mut self.bed[..len];
        bed.fill(0.0);
        for slot in &mut self.layers {
            let scratch = &mut self.scratch[..len];
            scratch.fill(0.0);
            slot.active = slot.source.render(scratch);
            for (frame_out, frame_in) in bed
                .as_chunks_mut::<CHANNELS>()
                .0
                .iter_mut()
                .zip(scratch.as_chunks::<CHANNELS>().0)
            {
                let g = slot.gain.next();
                frame_out[0] += frame_in[0] * g;
                frame_out[1] += frame_in[1] * g;
            }
        }

        let mut finished = false;
        for (frame_out, frame_bed) in out
            .as_chunks_mut::<CHANNELS>()
            .0
            .iter_mut()
            .zip(bed.as_chunks::<CHANNELS>().0)
        {
            let duck = self.duck.next();
            let master = self.master.next();
            let (mut l, mut r) = (frame_bed[0] * duck, frame_bed[1] * duck);
            if let Some(playing) = &mut self.current {
                if playing.preroll > 0 {
                    playing.preroll -= 1;
                } else if playing.pos + 1 < playing.alert.audio.len() {
                    l += playing.alert.audio[playing.pos];
                    r += playing.alert.audio[playing.pos + 1];
                    playing.pos += CHANNELS;
                } else {
                    finished = true;
                }
            }
            frame_out[0] = (l * master).clamp(-1.0, 1.0);
            frame_out[1] = (r * master).clamp(-1.0, 1.0);
        }
        if finished {
            self.finish_current(FinishReason::Completed);
        }

        if self.current.is_none() && self.tail_left > 0 {
            self.tail_left = self.tail_left.saturating_sub(len / CHANNELS);
            if self.tail_left == 0 && self.queue.is_empty() {
                self.ducking = None;
            }
        }

        self.levels
            .duck
            .store(self.duck.value.to_bits(), Ordering::Relaxed);
        self.levels
            .master
            .store(self.master.value.to_bits(), Ordering::Relaxed);
        let fg = self
            .layers
            .iter()
            .any(|s| s.layer == Layer::Foreground && s.active);
        self.levels.foreground_active.store(fg, Ordering::Relaxed);
    }

    fn handle_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Enqueue(alert) => {
                    // Critical alerts go ahead of every queued notification.
                    let index = match alert.priority {
                        AlertPriority::Critical => self
                            .queue
                            .iter()
                            .position(|a| a.priority == AlertPriority::Notification)
                            .unwrap_or(self.queue.len()),
                        AlertPriority::Notification => self.queue.len(),
                    };
                    self.queue.insert(index, alert);
                }
                Command::Skip => self.finish_current(FinishReason::Skipped),
                Command::Clear => {
                    self.finish_current(FinishReason::Cleared);
                    while let Some(alert) = self.queue.pop_front() {
                        self.emit(Event::Finished {
                            alert,
                            reason: FinishReason::Cleared,
                        });
                    }
                }
                Command::SetVolume(volume) => self.master_volume = volume.clamp(0.0, 1.0),
                Command::AddSource {
                    name,
                    layer,
                    source,
                } => self.add_source(name, layer, source),
                Command::RemoveSource(name) => self.remove_source(&name),
            }
        }
    }

    fn advance_queue(&mut self) {
        let preempt = matches!(
            (&self.current, self.queue.front()),
            (Some(p), Some(next))
                if p.alert.priority == AlertPriority::Notification
                    && next.priority == AlertPriority::Critical
        );
        if preempt && let Some(playing) = self.current.take() {
            let id = playing.alert.id.clone();
            let critical = self.queue.pop_front();
            self.queue.push_front(playing.alert);
            if let Some(critical) = critical {
                self.queue.push_front(critical);
            }
            self.emit(Event::Preempted { id });
        }

        if self.current.is_some() {
            return;
        }
        let Some(alert) = self.queue.pop_front() else {
            return;
        };
        self.ducking = Some(alert.priority);
        self.duck.target = self.duck_target();
        let preroll = if self.duck.settled() {
            0
        } else {
            self.attack_frames
        };
        self.emit(Event::Started {
            id: alert.id.clone(),
        });
        self.current = Some(Playing {
            alert,
            pos: 0,
            preroll,
        });
    }

    fn update_targets(&mut self) {
        self.duck.target = self.duck_target();
        self.master.target = match self.ducking {
            Some(AlertPriority::Critical) => self.master_volume.max(self.critical_min_volume),
            _ => self.master_volume,
        };
        let foreground = self
            .layers
            .iter()
            .any(|s| s.layer == Layer::Foreground && s.active);
        for slot in &mut self.layers {
            slot.gain.target = match slot.layer {
                Layer::Background if foreground => 0.0,
                _ => 1.0,
            };
        }
    }

    fn duck_target(&self) -> f32 {
        match self.ducking {
            None => 1.0,
            Some(AlertPriority::Notification) => self.notification_gain,
            Some(AlertPriority::Critical) => self.critical_gain,
        }
    }

    fn finish_current(&mut self, reason: FinishReason) {
        if let Some(playing) = self.current.take() {
            self.tail_left = self.release_delay_frames.max(1);
            self.emit(Event::Finished {
                alert: playing.alert,
                reason,
            });
        }
    }

    fn emit(&self, event: Event) {
        // Bounded and non-blocking; if the control side has stalled, the event is dropped.
        let _ = self.events.try_send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    const RATE: u32 = 1000;

    fn mixer() -> (Mixer, Sender<Command>, Receiver<Event>) {
        let (cmd_tx, cmd_rx) = bounded(16);
        let (ev_tx, ev_rx) = bounded(16);
        let cfg = MixerConfig {
            master_volume: 1.0,
            attack_ms: 10,
            release_ms: 10,
            release_delay_ms: 10,
            crossfade_ms: 10,
            ..MixerConfig::default()
        };
        let mut m = Mixer::new(RATE, &cfg, cmd_rx, ev_tx, Arc::default());
        m.add_source(
            "bg".into(),
            Layer::Background,
            Box::new(LoopSource::new(vec![0.5; 2], 1.0)),
        );
        (m, cmd_tx, ev_rx)
    }

    fn alert(id: &str, priority: AlertPriority, frames: usize) -> PreparedAlert {
        PreparedAlert {
            id: id.into(),
            priority,
            audio: Arc::new(vec![0.25; frames * CHANNELS]),
        }
    }

    fn render(m: &mut Mixer, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * CHANNELS];
        m.render(&mut out);
        out
    }

    #[test]
    fn notification_ducks_the_bed_and_releases_after() {
        let (mut m, tx, events) = mixer();
        render(&mut m, 100);
        assert!((render(&mut m, 1)[0] - 0.5).abs() < 1e-3, "bed fades in");

        tx.send(Command::Enqueue(alert(
            "a",
            AlertPriority::Notification,
            50,
        )))
        .unwrap();
        let out = render(&mut m, 40);
        let ducked = 0.5 * db_to_gain(-15.0);
        // After the 10-frame preroll the alert plays over the ducked bed.
        assert!((out[30 * CHANNELS] - (ducked + 0.25)).abs() < 1e-3);
        assert!(matches!(events.try_recv(), Ok(Event::Started { .. })));

        render(&mut m, 100);
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Finished {
                reason: FinishReason::Completed,
                ..
            })
        ));
        render(&mut m, 50);
        assert!(
            (render(&mut m, 1)[0] - 0.5).abs() < 1e-3,
            "bed comes back up"
        );
    }

    #[test]
    fn critical_preempts_notification() {
        let (mut m, tx, events) = mixer();
        tx.send(Command::Enqueue(alert(
            "n",
            AlertPriority::Notification,
            500,
        )))
        .unwrap();
        render(&mut m, 50);
        tx.send(Command::Enqueue(alert("c", AlertPriority::Critical, 50)))
            .unwrap();
        render(&mut m, 1);

        let ids: Vec<String> = events
            .try_iter()
            .map(|e| match e {
                Event::Started { id } => format!("start:{id}"),
                Event::Preempted { id } => format!("preempt:{id}"),
                Event::Finished { alert, .. } => format!("finish:{}", alert.id),
                Event::SourceRemoved(_) => "removed".into(),
            })
            .collect();
        assert_eq!(ids, ["start:n", "preempt:n", "start:c"]);

        // The notification plays again once the critical alert is done.
        render(&mut m, 100);
        render(&mut m, 100);
        let ids: Vec<String> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::Started { id } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["n"]);
    }

    #[test]
    fn clear_drops_queue() {
        let (mut m, tx, events) = mixer();
        for id in ["a", "b", "c"] {
            tx.send(Command::Enqueue(alert(
                id,
                AlertPriority::Notification,
                100,
            )))
            .unwrap();
        }
        render(&mut m, 5);
        tx.send(Command::Clear).unwrap();
        render(&mut m, 5);
        let cleared = events
            .try_iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Finished {
                        reason: FinishReason::Cleared,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(cleared, 3);
    }
}
