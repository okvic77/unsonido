//! Turns alert requests into rendered clips, applies policy (quiet hours, dedupe, queue limits),
//! hands them to the mixer, and tracks what is queued and playing.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{Local, NaiveTime};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};

use crate::audio;
use crate::config::{AlertsConfig, QuietHours};
use crate::engine::{AlertPriority, Command, Event, FinishReason, Levels, PreparedAlert};
use crate::tts::{SpeechRequest, Tts};

const HISTORY_LEN: usize = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRequest {
    /// Stable id for deduplication, e.g. "calendar:<event-id>:5min".
    pub id: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: AlertPriority,
    /// Text to speak.
    pub text: Option<String>,
    /// TTS provider name from the config; the default provider when omitted.
    pub provider: Option<String>,
    pub voice: Option<String>,
    /// http(s) URL of an audio file to play.
    pub url: Option<String>,
    /// How many times to play the alert.
    pub repeat: Option<u32>,
    /// Play a chime first. Defaults to true.
    pub chime: Option<bool>,
}

fn default_priority() -> AlertPriority {
    AlertPriority::Notification
}

pub enum Content {
    Text {
        text: String,
        provider: Option<String>,
        voice: Option<String>,
    },
    Url(String),
    Audio {
        bytes: Vec<u8>,
        extension: Option<String>,
    },
    ChimeOnly,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Queued {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        duration_ms: u64,
    },
    Suppressed {
        id: String,
        reason: &'static str,
    },
}

#[derive(Debug)]
pub enum AlertError {
    Invalid(String),
    QueueFull,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for AlertError {
    fn from(err: anyhow::Error) -> Self {
        Self::Failed(err)
    }
}

#[derive(Clone, Serialize)]
pub struct AlertInfo {
    pub id: String,
    pub priority: AlertPriority,
    pub label: String,
}

#[derive(Clone, Serialize)]
pub struct HistoryEntry {
    #[serde(flatten)]
    pub alert: AlertInfo,
    pub result: FinishReason,
    pub at: String,
}

#[derive(Serialize)]
pub struct Status {
    pub sample_rate: u32,
    pub volume: f32,
    pub output_gain: f32,
    pub bed_gain: f32,
    pub foreground_active: bool,
    pub quiet_hours_active: bool,
    pub current: Option<AlertInfo>,
    pub queue: Vec<AlertInfo>,
    pub history: Vec<HistoryEntry>,
}

#[derive(Default)]
struct State {
    current: Option<AlertInfo>,
    queue: Vec<AlertInfo>,
    history: VecDeque<HistoryEntry>,
    /// Explicit ids seen recently, for deduplication.
    recent: HashMap<String, Instant>,
    volume: f32,
}

pub struct AlertService {
    cfg: AlertsConfig,
    quiet_hours: Option<(NaiveTime, NaiveTime)>,
    rate: u32,
    tts: Arc<Tts>,
    http: reqwest::Client,
    commands: Sender<Command>,
    levels: Arc<Levels>,
    state: Mutex<State>,
    /// True while a critical alert is playing or queued.
    critical: tokio::sync::watch::Sender<bool>,
}

impl AlertService {
    pub fn new(
        cfg: AlertsConfig,
        rate: u32,
        volume: f32,
        tts: Arc<Tts>,
        commands: Sender<Command>,
        levels: Arc<Levels>,
        critical: tokio::sync::watch::Sender<bool>,
    ) -> anyhow::Result<Arc<Self>> {
        let quiet_hours = cfg
            .quiet_hours
            .as_ref()
            .map(parse_quiet_hours)
            .transpose()?;
        let state = State {
            volume,
            ..State::default()
        };
        Ok(Arc::new(Self {
            cfg,
            quiet_hours,
            rate,
            tts,
            http: reqwest::Client::builder()
                .user_agent("unsonido")
                .timeout(Duration::from_secs(30))
                .build()?,
            commands,
            levels,
            state: Mutex::new(state),
            critical,
        }))
    }

    pub fn tts(&self) -> &Tts {
        &self.tts
    }

    /// Consume mixer events on a dedicated thread. Finished clips are dropped here,
    /// off the audio thread.
    pub fn spawn_event_pump(self: &Arc<Self>, events: Receiver<Event>) {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("mixer-events".into())
            .spawn(move || {
                for event in events {
                    this.on_event(event);
                }
            })
            .expect("spawning event thread");
    }

    fn on_event(&self, event: Event) {
        let mut state = self.state.lock().unwrap();
        match event {
            Event::Started { id } => {
                if let Some(pos) = state.queue.iter().position(|a| a.id == id) {
                    let info = state.queue.remove(pos);
                    tracing::info!(id = %info.id, priority = ?info.priority, "alert playing");
                    state.current = Some(info);
                }
            }
            Event::Preempted { id } => {
                if let Some(info) = state.current.take_if(|c| c.id == id) {
                    tracing::info!(id = %id, "alert preempted, requeued");
                    state.queue.insert(0, info);
                }
            }
            Event::Finished { alert, reason } => {
                let info = if state.current.as_ref().is_some_and(|c| c.id == alert.id) {
                    state.current.take()
                } else {
                    state
                        .queue
                        .iter()
                        .position(|a| a.id == alert.id)
                        .map(|pos| state.queue.remove(pos))
                };
                if let Some(info) = info {
                    tracing::info!(id = %info.id, ?reason, "alert finished");
                    state.history.push_front(HistoryEntry {
                        alert: info,
                        result: reason,
                        at: Local::now().to_rfc3339(),
                    });
                    state.history.truncate(HISTORY_LEN);
                }
            }
            // Dropping it here (off the audio thread) also stops its decoder thread.
            Event::SourceRemoved(source) => drop(source),
        }
        let critical = state
            .current
            .iter()
            .chain(&state.queue)
            .any(|a| a.priority == AlertPriority::Critical);
        self.critical
            .send_if_modified(|active| std::mem::replace(active, critical) != critical);
    }

    pub async fn submit(&self, request: AlertRequest) -> Result<Outcome, AlertError> {
        let content = match (request.text, request.url) {
            (Some(_), Some(_)) => {
                return Err(AlertError::Invalid(
                    "use either `text` or `url`, not both".into(),
                ));
            }
            (Some(text), None) => {
                if text.trim().is_empty() {
                    return Err(AlertError::Invalid("`text` is empty".into()));
                }
                Content::Text {
                    text,
                    provider: request.provider,
                    voice: request.voice,
                }
            }
            (None, Some(url)) => Content::Url(url),
            (None, None) if request.chime != Some(false) => Content::ChimeOnly,
            (None, None) => {
                return Err(AlertError::Invalid(
                    "nothing to play: give `text` or `url`".into(),
                ));
            }
        };
        self.submit_content(
            request.id,
            request.priority,
            content,
            request.repeat,
            request.chime.unwrap_or(true),
        )
        .await
    }

    pub async fn submit_content(
        &self,
        id: Option<String>,
        priority: AlertPriority,
        content: Content,
        repeat: Option<u32>,
        chime: bool,
    ) -> Result<Outcome, AlertError> {
        let repeat = repeat.unwrap_or(1);
        if repeat == 0 || repeat > self.cfg.max_repeat {
            return Err(AlertError::Invalid(format!(
                "`repeat` must be between 1 and {}",
                self.cfg.max_repeat
            )));
        }
        if let Content::Text {
            provider: Some(p), ..
        } = &content
            && !self.tts.has_provider(p)
        {
            return Err(AlertError::Invalid(format!("unknown tts provider {p:?}")));
        }
        let explicit_id = id.is_some();
        let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if priority == AlertPriority::Notification && self.quiet_hours_active() {
            return Ok(Outcome::Suppressed {
                id,
                reason: "quiet_hours",
            });
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.queue.len() >= self.cfg.max_queue {
                return Err(AlertError::QueueFull);
            }
            if explicit_id {
                let window = Duration::from_secs(self.cfg.dedupe_seconds);
                state.recent.retain(|_, seen| seen.elapsed() < window);
                if state.recent.contains_key(&id) {
                    return Ok(Outcome::Suppressed {
                        id,
                        reason: "duplicate",
                    });
                }
                // Reserve the id now so concurrent duplicates are rejected while this one renders.
                state.recent.insert(id.clone(), Instant::now());
            }
        }

        let label = match &content {
            Content::Text { text, .. } => text.chars().take(120).collect(),
            Content::Url(url) => url.clone(),
            Content::Audio { .. } => "uploaded audio".into(),
            Content::ChimeOnly => "chime".into(),
        };
        let rendered = self.render(priority, content, repeat, chime).await;
        let (audio, provider) = match rendered {
            Ok(r) => r,
            Err(err) => {
                if explicit_id {
                    self.state.lock().unwrap().recent.remove(&id);
                }
                return Err(err);
            }
        };

        let duration_ms = (audio.len() / audio::CHANNELS) as u64 * 1000 / self.rate as u64;
        let info = AlertInfo {
            id: id.clone(),
            priority,
            label,
        };
        // Track it before the mixer can report it started.
        self.state.lock().unwrap().queue.push(info);
        let prepared = PreparedAlert {
            id: id.clone(),
            priority,
            audio: Arc::new(audio),
        };
        if let Err(err) = self.commands.try_send(Command::Enqueue(prepared)) {
            let mut state = self.state.lock().unwrap();
            state.queue.retain(|a| a.id != id);
            return Err(match err {
                TrySendError::Full(_) => AlertError::QueueFull,
                TrySendError::Disconnected(_) => {
                    AlertError::Failed(anyhow!("audio engine stopped"))
                }
            });
        }
        tracing::info!(id = %id, ?priority, duration_ms, "alert queued");
        Ok(Outcome::Queued {
            id,
            provider,
            duration_ms,
        })
    }

    /// Build the full clip: [chime, pause, content] repeated, with gaps in between.
    async fn render(
        &self,
        priority: AlertPriority,
        content: Content,
        repeat: u32,
        chime: bool,
    ) -> Result<(Vec<f32>, Option<String>), AlertError> {
        let (encoded, provider) = match content {
            Content::Text {
                text,
                provider,
                voice,
            } => {
                let request = SpeechRequest {
                    text: &text,
                    voice: voice.as_deref(),
                };
                let (speech, used) = self.tts.synthesize(provider.as_deref(), &request).await?;
                (Some((speech.bytes, Some(speech.extension))), Some(used))
            }
            Content::Url(url) => (Some(self.fetch(&url).await?), None),
            Content::Audio { bytes, extension } => {
                if bytes.len() > self.cfg.max_audio_bytes {
                    return Err(AlertError::Invalid("audio is too large".into()));
                }
                (Some((bytes, extension)), None)
            }
            Content::ChimeOnly => (None, None),
        };

        let rate = self.rate;
        let target = self.cfg.target_dbfs;
        let clip = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<f32>> {
            let body = match encoded {
                Some((bytes, ext)) => {
                    let mut pcm = audio::decode(bytes, ext.as_deref(), rate)?;
                    audio::normalize(&mut pcm, target);
                    Some(pcm)
                }
                None => None,
            };
            let chime_pcm = match priority {
                AlertPriority::Notification => audio::notification_chime(rate),
                AlertPriority::Critical => audio::critical_chime(rate),
            };
            let mut clip = Vec::new();
            for i in 0..repeat {
                if i > 0 {
                    clip.extend(audio::silence(rate, 800));
                }
                if chime || body.is_none() {
                    clip.extend_from_slice(&chime_pcm);
                    if body.is_some() {
                        clip.extend(audio::silence(rate, 250));
                    }
                }
                if let Some(body) = &body {
                    clip.extend_from_slice(body);
                }
            }
            Ok(clip)
        })
        .await
        .map_err(|e| anyhow!("render task failed: {e}"))??;
        Ok((clip, provider))
    }

    async fn fetch(&self, url: &str) -> Result<(Vec<u8>, Option<String>), AlertError> {
        let parsed =
            reqwest::Url::parse(url).map_err(|e| AlertError::Invalid(format!("bad url: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AlertError::Invalid(
                "only http and https urls are supported".into(),
            ));
        }
        let extension = parsed
            .path_segments()
            .and_then(|mut s| s.next_back())
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext.to_lowercase());
        let response = self
            .http
            .get(parsed)
            .send()
            .await
            .context("fetching audio url")?;
        let response = crate::tts::check_response(response).await?;
        if response
            .content_length()
            .is_some_and(|len| len as usize > self.cfg.max_audio_bytes)
        {
            return Err(AlertError::Invalid("audio at url is too large".into()));
        }
        let bytes = response.bytes().await.context("downloading audio")?;
        if bytes.len() > self.cfg.max_audio_bytes {
            return Err(AlertError::Invalid("audio at url is too large".into()));
        }
        Ok((bytes.to_vec(), extension))
    }

    pub fn skip(&self) {
        let _ = self.commands.try_send(Command::Skip);
    }

    pub fn clear(&self) {
        let _ = self.commands.try_send(Command::Clear);
    }

    pub fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.state.lock().unwrap().volume = volume;
        let _ = self.commands.try_send(Command::SetVolume(volume));
    }

    pub fn status(&self) -> Status {
        let state = self.state.lock().unwrap();
        Status {
            sample_rate: self.rate,
            volume: state.volume,
            output_gain: self.levels.master(),
            bed_gain: self.levels.duck(),
            foreground_active: self.levels.foreground_active(),
            quiet_hours_active: self.quiet_hours_active(),
            current: state.current.clone(),
            queue: state.queue.clone(),
            history: state.history.iter().cloned().collect(),
        }
    }

    fn quiet_hours_active(&self) -> bool {
        self.quiet_hours
            .is_some_and(|(start, end)| in_window(Local::now().time(), start, end))
    }
}

fn parse_quiet_hours(q: &QuietHours) -> anyhow::Result<(NaiveTime, NaiveTime)> {
    let parse = |s: &str| {
        NaiveTime::parse_from_str(s, "%H:%M")
            .with_context(|| format!("quiet_hours time {s:?} must be HH:MM"))
    };
    Ok((parse(&q.start)?, parse(&q.end)?))
}

/// Whether `now` falls in [start, end), where the window may wrap past midnight.
fn in_window(now: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    #[test]
    fn quiet_window_wraps_midnight() {
        assert!(in_window(t("23:30"), t("22:00"), t("07:00")));
        assert!(in_window(t("06:59"), t("22:00"), t("07:00")));
        assert!(!in_window(t("07:00"), t("22:00"), t("07:00")));
        assert!(!in_window(t("12:00"), t("22:00"), t("07:00")));
        assert!(in_window(t("13:00"), t("12:00"), t("14:00")));
    }
}
