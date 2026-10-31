//! Turns alert requests into rendered clips, applies policy (quiet hours, dedupe, queue limits),
//! hands them to the mixer, and tracks what is queued and playing.
//!
//! Alerts can be posted anonymously, or applied kubectl-style into a namespace under a name
//! (`calendar`, `agents`, `script`, ...). A named alert replaces a pending one with the same
//! name, can be deleted before it plays, and auto-destructs out of its namespace once played.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Set for alerts applied kubectl-style into a namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct HistoryEntry {
    #[serde(flatten)]
    pub alert: AlertInfo,
    pub result: FinishReason,
    pub at: String,
}

/// A named alert waiting to play, by namespace then name; auto-destructs once played.
#[derive(Clone, Serialize)]
pub struct StoredAlert {
    #[serde(flatten)]
    pub info: AlertInfo,
    /// "playing" or "pending".
    pub state: StoredState,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoredState {
    Playing,
    Pending,
}

#[derive(Serialize)]
pub struct NamespaceInfo {
    pub name: String,
    pub alerts: Vec<StoredAlert>,
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
    pub namespaces: Vec<NamespaceInfo>,
}

#[derive(Default)]
struct State {
    current: Option<AlertInfo>,
    queue: Vec<AlertInfo>,
    history: VecDeque<HistoryEntry>,
    /// Explicit ids seen recently, for deduplication.
    recent: HashMap<String, Instant>,
    /// Named alerts waiting to play, by namespace then name. Entries auto-destruct
    /// when the alert finishes.
    namespaces: BTreeMap<String, BTreeMap<String, AlertInfo>>,
    volume: f32,
}

/// How a submission is tracked: anonymous, or kubectl-style inside a namespace.
enum Identity {
    /// Deduplicated on an explicit id when given; anonymous otherwise.
    Plain { id: Option<String> },
    /// Registered under (namespace, name). Applying replaces a pending alert with
    /// that name; the entry auto-destructs once the alert has played.
    Named { namespace: String, name: String },
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
    /// Makes each named alert's engine id unique, so a replaced alert can be
    /// cancelled while its replacement is already queued.
    named_seq: AtomicU64,
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
            named_seq: AtomicU64::new(0),
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
                // Auto-destruct: a played alert leaves its namespace, and an empty
                // namespace disappears with it.
                state.namespaces.retain(|_, alerts| {
                    alerts.retain(|_, a| a.id != alert.id);
                    !alerts.is_empty()
                });
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
        let content = content_of(&request)?;
        self.enqueue_alert(
            Identity::Plain { id: request.id },
            request.priority,
            content,
            request.repeat,
            request.chime.unwrap_or(true),
        )
        .await
    }

    /// kubectl-style: apply an alert under (namespace, name). A pending alert with the
    /// same name is replaced; the entry auto-destructs once the alert has played.
    pub async fn apply(
        &self,
        namespace: String,
        name: String,
        request: AlertRequest,
    ) -> Result<Outcome, AlertError> {
        validate_name(&namespace, "namespace")?;
        validate_name(&name, "name")?;
        let content = content_of(&request)?;
        self.enqueue_alert(
            Identity::Named { namespace, name },
            request.priority,
            content,
            request.repeat,
            request.chime.unwrap_or(true),
        )
        .await
    }

    /// Like [`apply`](Self::apply), but with already-built content (uploaded audio).
    pub async fn apply_content(
        &self,
        namespace: String,
        name: String,
        priority: AlertPriority,
        content: Content,
        repeat: Option<u32>,
        chime: bool,
    ) -> Result<Outcome, AlertError> {
        validate_name(&namespace, "namespace")?;
        validate_name(&name, "name")?;
        self.enqueue_alert(
            Identity::Named { namespace, name },
            priority,
            content,
            repeat,
            chime,
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
        self.enqueue_alert(Identity::Plain { id }, priority, content, repeat, chime)
            .await
    }

    async fn enqueue_alert(
        &self,
        identity: Identity,
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
        let (id, explicit_id) = match &identity {
            Identity::Plain { id } => (
                id.clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                id.is_some(),
            ),
            // "<ns>/<name>#<n>" reads well in logs and stays unique across replaces.
            Identity::Named { namespace, name } => {
                let n = self.named_seq.fetch_add(1, Ordering::Relaxed);
                (format!("{namespace}/{name}#{n}"), false)
            }
        };

        if priority == AlertPriority::Notification && self.quiet_hours_active() {
            return Ok(Outcome::Suppressed {
                id,
                reason: "quiet_hours",
            });
        }

        // Replace: a pending alert with the same name is cancelled first. If the new
        // render then fails, the old one is already gone — last apply wins.
        if let Identity::Named { namespace, name } = &identity {
            let old = {
                let mut state = self.state.lock().unwrap();
                let old = state
                    .namespaces
                    .get_mut(namespace)
                    .and_then(|alerts| alerts.remove(name));
                if state
                    .namespaces
                    .get(namespace)
                    .is_some_and(|alerts| alerts.is_empty())
                {
                    state.namespaces.remove(namespace);
                }
                old
            };
            if let Some(old) = old {
                let _ = self.commands.try_send(Command::CancelAlert(old.id));
            }
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
        let (namespace, name) = match &identity {
            Identity::Named { namespace, name } => (Some(namespace.clone()), Some(name.clone())),
            Identity::Plain { .. } => (None, None),
        };
        let info = AlertInfo {
            id: id.clone(),
            priority,
            label,
            namespace,
            name,
        };
        // Track it before the mixer can report it started.
        {
            let mut state = self.state.lock().unwrap();
            state.queue.push(info.clone());
            if let Identity::Named { namespace, name } = &identity {
                state
                    .namespaces
                    .entry(namespace.clone())
                    .or_default()
                    .insert(name.clone(), info.clone());
            }
        }
        let prepared = PreparedAlert {
            id: id.clone(),
            priority,
            audio: Arc::new(audio),
        };
        if let Err(err) = self.commands.try_send(Command::Enqueue(prepared)) {
            let mut state = self.state.lock().unwrap();
            state.queue.retain(|a| a.id != id);
            if let Identity::Named { namespace, name } = &identity
                && let Some(alerts) = state.namespaces.get_mut(namespace)
            {
                alerts.remove(name);
                if alerts.is_empty() {
                    state.namespaces.remove(namespace);
                }
            }
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
            namespaces: namespaces_view(&state),
        }
    }

    /// Every namespace and its pending alerts, e.g. `calendar`, `agents`, `script`.
    pub fn list_namespaces(&self) -> Vec<NamespaceInfo> {
        let state = self.state.lock().unwrap();
        namespaces_view(&state)
    }

    /// The alerts of one namespace, playing ones first. None when it does not exist.
    pub fn namespace_alerts(&self, namespace: &str) -> Option<Vec<StoredAlert>> {
        let state = self.state.lock().unwrap();
        state
            .namespaces
            .get(namespace)
            .map(|alerts| stored_alerts(&state, alerts))
    }

    /// One stored alert. None when it does not exist.
    pub fn stored_alert(&self, namespace: &str, name: &str) -> Option<StoredAlert> {
        let state = self.state.lock().unwrap();
        state
            .namespaces
            .get(namespace)
            .and_then(|alerts| alerts.get(name))
            .map(|info| stored_alert(&state, info))
    }

    /// Delete a stored alert: it stops playing or leaves the queue. False when it
    /// does not exist.
    pub fn delete_alert(&self, namespace: &str, name: &str) -> bool {
        let id = {
            let mut state = self.state.lock().unwrap();
            let removed = state
                .namespaces
                .get_mut(namespace)
                .and_then(|alerts| alerts.remove(name));
            if removed.is_some()
                && state
                    .namespaces
                    .get(namespace)
                    .is_some_and(|alerts| alerts.is_empty())
            {
                state.namespaces.remove(namespace);
            }
            removed
        };
        if let Some(info) = id {
            let _ = self.commands.try_send(Command::CancelAlert(info.id));
            true
        } else {
            false
        }
    }

    /// Delete a namespace and every alert in it. False when it does not exist.
    pub fn delete_namespace(&self, namespace: &str) -> bool {
        let ids = {
            let mut state = self.state.lock().unwrap();
            state
                .namespaces
                .remove(namespace)
                .map(|alerts| alerts.into_values().map(|a| a.id).collect::<Vec<_>>())
        };
        match ids {
            Some(ids) => {
                for id in ids {
                    let _ = self.commands.try_send(Command::CancelAlert(id));
                }
                true
            }
            None => false,
        }
    }

    fn quiet_hours_active(&self) -> bool {
        self.quiet_hours
            .is_some_and(|(start, end)| in_window(Local::now().time(), start, end))
    }
}

/// Build the content of a request: text to speak, an audio url, or a chime only.
fn content_of(request: &AlertRequest) -> Result<Content, AlertError> {
    match (&request.text, &request.url) {
        (Some(_), Some(_)) => Err(AlertError::Invalid(
            "use either `text` or `url`, not both".into(),
        )),
        (Some(text), None) => {
            if text.trim().is_empty() {
                return Err(AlertError::Invalid("`text` is empty".into()));
            }
            Ok(Content::Text {
                text: text.clone(),
                provider: request.provider.clone(),
                voice: request.voice.clone(),
            })
        }
        (None, Some(url)) => Ok(Content::Url(url.clone())),
        (None, None) if request.chime != Some(false) => Ok(Content::ChimeOnly),
        (None, None) => Err(AlertError::Invalid(
            "nothing to play: give `text` or `url`".into(),
        )),
    }
}

/// Namespaces and names may be anything except empty, too long, or containing '/'.
fn validate_name(value: &str, what: &str) -> Result<(), AlertError> {
    if value.trim().is_empty() {
        return Err(AlertError::Invalid(format!("`{what}` must not be empty")));
    }
    if value.len() > 100 {
        return Err(AlertError::Invalid(format!("`{what}` is too long")));
    }
    if value.contains('/') {
        return Err(AlertError::Invalid(format!(
            "`{what}` must not contain '/'"
        )));
    }
    Ok(())
}

/// Snapshot of the namespaces for the status and the listing endpoints.
fn namespaces_view(state: &State) -> Vec<NamespaceInfo> {
    state
        .namespaces
        .iter()
        .map(|(name, alerts)| NamespaceInfo {
            name: name.clone(),
            alerts: stored_alerts(state, alerts),
        })
        .collect()
}

fn stored_alerts(state: &State, alerts: &BTreeMap<String, AlertInfo>) -> Vec<StoredAlert> {
    alerts
        .values()
        .map(|info| stored_alert(state, info))
        .collect()
}

fn stored_alert(state: &State, info: &AlertInfo) -> StoredAlert {
    let state_ = if state.current.as_ref().is_some_and(|c| c.id == info.id) {
        StoredState::Playing
    } else {
        StoredState::Pending
    };
    StoredAlert {
        info: info.clone(),
        state: state_,
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

    fn request(json: &str) -> AlertRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn named_alerts_validate_their_namespace_and_name() {
        assert!(validate_name("calendar", "namespace").is_ok());
        assert!(validate_name("a b.c-d_e", "name").is_ok());
        assert!(validate_name("", "name").is_err());
        assert!(validate_name("  ", "name").is_err());
        assert!(validate_name("has/slash", "name").is_err());
        assert!(validate_name(&"x".repeat(101), "name").is_err());
    }

    #[test]
    fn content_of_builds_text_url_and_chime() {
        assert!(matches!(
            content_of(&request(r#"{"text":"hello"}"#)).unwrap(),
            Content::Text { .. }
        ));
        assert!(matches!(
            content_of(&request(r#"{"url":"https://x.example/a.mp3"}"#)).unwrap(),
            Content::Url(_)
        ));
        assert!(matches!(
            content_of(&request("{}")).unwrap(),
            Content::ChimeOnly
        ));
        // Both, or chime=false with nothing else, are rejected.
        assert!(content_of(&request(r#"{"text":"hi","url":"https://x.example/a.mp3"}"#)).is_err());
        assert!(content_of(&request(r#"{"chime":false}"#)).is_err());
        assert!(content_of(&request(r#"{"text":"  "}"#)).is_err());
    }
}
