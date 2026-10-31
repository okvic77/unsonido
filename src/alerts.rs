//! Turns alert requests into rendered clips, applies policy (quiet hours, dedupe, queue limits),
//! hands them to the mixer, and tracks what is queued and playing.
//!
//! Alerts can be posted anonymously, or applied kubectl-style into a namespace under a name
//! (`calendar`, `agents`, `script`, ...). A named alert replaces a pending one with the same
//! name, can be deleted before it plays, and auto-destructs out of its namespace once played.
//! Applying with `at` schedules it for a future moment instead; the dispatcher hands each
//! alert to the mixer as it falls due, and a namespace can be synced wholesale: apply a set
//! of alerts, prune the rest.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{DateTime, Local, MappedLocalTime, NaiveDate, NaiveDateTime, NaiveTime};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::audio;
use crate::config::{AlertsConfig, QuietHours};
use crate::engine::{AlertPriority, Command, Event, FinishReason, Levels, PreparedAlert};
use crate::tts::{SpeechRequest, Tts};

const HISTORY_LEN: usize = 20;
/// How often the scheduler looks for alerts that fell due.
const TICK: Duration = Duration::from_secs(1);
/// A due alert whose rendering keeps failing is retried this often, then dropped.
const DISPATCH_ATTEMPTS: u8 = 3;
const RETRY_AFTER: chrono::Duration = chrono::Duration::seconds(30);

#[derive(Debug, Clone, Deserialize)]
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
    /// Play at this future time instead of right away. Needs a namespace and name.
    /// RFC 3339 or "YYYY-MM-DD HH:MM" in local time; a past time plays immediately.
    pub at: Option<String>,
}

fn default_priority() -> AlertPriority {
    AlertPriority::Notification
}

#[derive(Clone, PartialEq)]
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

/// What to play, in a form that can wait in a namespace, be compared for idempotence,
/// and be rendered whenever the moment comes.
#[derive(Clone, PartialEq)]
pub struct AlertSpec {
    pub content: Content,
    pub repeat: u32,
    pub chime: bool,
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
    Scheduled {
        id: String,
        due_at: String,
    },
    /// An identical alert is already stored under this name.
    Unchanged {
        id: String,
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

impl AlertError {
    pub fn message(&self) -> String {
        match self {
            AlertError::Invalid(message) => message.clone(),
            AlertError::QueueFull => "alert queue is full".into(),
            AlertError::Failed(err) => format!("{err:#}"),
        }
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

/// One alert applied into a namespace, as the API and UI see it.
#[derive(Clone, Serialize)]
pub struct StoredAlert {
    pub namespace: String,
    pub name: String,
    pub priority: AlertPriority,
    pub label: String,
    /// Engine id, once the alert has been handed off to play.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub state: StoredState,
    /// Present while the alert waits for its moment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
}

#[derive(Clone, Copy, Serialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum StoredState {
    Playing,
    Pending,
    Scheduled,
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

/// Result of syncing a whole namespace: per-alert outcomes, plus what was pruned.
#[derive(Serialize)]
pub struct SyncReport {
    pub namespace: String,
    pub results: BTreeMap<String, serde_json::Value>,
    pub pruned: Vec<String>,
}

/// One alert applied into a namespace. While scheduled it waits with `due_at` set and no
/// engine id; the moment it is handed off, an engine id is reserved so a newer apply or
/// a delete can still cancel it while it renders.
#[derive(Clone)]
struct StoredEntry {
    id: Option<String>,
    priority: AlertPriority,
    label: String,
    spec: AlertSpec,
    due_at: Option<DateTime<Local>>,
    attempts: u8,
}

#[derive(Default)]
struct State {
    current: Option<AlertInfo>,
    queue: Vec<AlertInfo>,
    history: VecDeque<HistoryEntry>,
    /// Explicit ids seen recently, for deduplication.
    recent: HashMap<String, Instant>,
    /// Alerts applied into namespaces, by namespace then name. Entries auto-destruct
    /// when the alert finishes playing.
    namespaces: BTreeMap<String, BTreeMap<String, StoredEntry>>,
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

    /// Wake every second and hand alerts that fell due to the mixer.
    pub fn spawn_scheduler(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            loop {
                tick.tick().await;
                this.dispatch_due().await;
            }
        });
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
                    alerts.retain(|_, a| a.id.as_deref() != Some(alert.id.as_str()));
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

    /// Post an anonymous alert, deduplicated on `id`. Anonymous alerts cannot be
    /// scheduled; `at` belongs to the named, kubectl-style ones.
    pub async fn submit(&self, request: AlertRequest) -> Result<Outcome, AlertError> {
        if request.at.is_some() {
            return Err(AlertError::Invalid(
                "`at` schedules an alert, which needs a namespace and name; use PUT /namespaces/{ns}/alerts/{name}"
                    .into(),
            ));
        }
        let content = content_of(&request)?;
        self.enqueue_alert(
            request.id,
            request.priority,
            content,
            request.repeat,
            request.chime.unwrap_or(true),
        )
        .await
    }

    /// kubectl-style: apply an alert under (namespace, name). It plays immediately, or at
    /// `at` when given. Applying again replaces the stored alert with the same name — an
    /// identical apply is a no-op — and the entry auto-destructs once the alert has played.
    pub async fn apply(
        &self,
        namespace: String,
        name: String,
        request: AlertRequest,
    ) -> Result<Outcome, AlertError> {
        validate_name(&namespace, "namespace")?;
        validate_name(&name, "name")?;
        let spec = AlertSpec {
            content: content_of(&request)?,
            repeat: request.repeat.unwrap_or(1),
            chime: request.chime.unwrap_or(true),
        };
        let at = match request.at {
            Some(value) => Some(parse_at(&value)?),
            None => None,
        };
        self.apply_spec(namespace, name, request.priority, spec, at)
            .await
    }

    /// Like [`apply`](Self::apply), but with already-built content (uploaded audio).
    pub async fn apply_content(
        &self,
        namespace: String,
        name: String,
        priority: AlertPriority,
        spec: AlertSpec,
        at: Option<String>,
    ) -> Result<Outcome, AlertError> {
        validate_name(&namespace, "namespace")?;
        validate_name(&name, "name")?;
        let at = match at {
            Some(value) => Some(parse_at(&value)?),
            None => None,
        };
        self.apply_spec(namespace, name, priority, spec, at).await
    }

    /// Replace a whole namespace at once: apply every alert in `alerts` by name, then prune
    /// the stored entries that are not in the set — the kubectl "apply a folder, delete the
    /// rest" sync.
    pub async fn sync_namespace(
        &self,
        namespace: String,
        alerts: BTreeMap<String, AlertRequest>,
    ) -> Result<SyncReport, AlertError> {
        validate_name(&namespace, "namespace")?;
        let mut results = BTreeMap::new();
        for (name, request) in &alerts {
            let result = match validate_name(name, "name") {
                Err(err) => Err(err),
                Ok(()) => {
                    self.apply(namespace.clone(), name.clone(), request.clone())
                        .await
                }
            };
            let value = match result {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or_default(),
                Err(err) => json!({ "error": err.message() }),
            };
            results.insert(name.clone(), value);
        }
        // Prune: entries not in the applied set are removed.
        let (pruned, cancelled) = {
            let mut state = self.state.lock().unwrap();
            let mut pruned = Vec::new();
            let mut cancelled = Vec::new();
            if let Some(entries) = state.namespaces.get_mut(&namespace) {
                for name in entries.keys().cloned().collect::<Vec<_>>() {
                    if alerts.contains_key(&name) {
                        continue;
                    }
                    if let Some(entry) = entries.remove(&name) {
                        pruned.push(name);
                        if let Some(id) = entry.id {
                            cancelled.push(id);
                        }
                    }
                }
                if entries.is_empty() {
                    state.namespaces.remove(&namespace);
                }
            }
            (pruned, cancelled)
        };
        for id in cancelled {
            let _ = self.commands.try_send(Command::CancelAlert(id));
        }
        Ok(SyncReport {
            namespace,
            results,
            pruned,
        })
    }

    pub async fn submit_content(
        &self,
        id: Option<String>,
        priority: AlertPriority,
        content: Content,
        repeat: Option<u32>,
        chime: bool,
    ) -> Result<Outcome, AlertError> {
        self.enqueue_alert(id, priority, content, repeat, chime)
            .await
    }

    /// The shared body of apply: validate, store the entry (replacing the old one), then
    /// either schedule it or render and hand it to the mixer right now.
    async fn apply_spec(
        &self,
        namespace: String,
        name: String,
        priority: AlertPriority,
        spec: AlertSpec,
        at: Option<DateTime<Local>>,
    ) -> Result<Outcome, AlertError> {
        if spec.repeat == 0 || spec.repeat > self.cfg.max_repeat {
            return Err(AlertError::Invalid(format!(
                "`repeat` must be between 1 and {}",
                self.cfg.max_repeat
            )));
        }
        if let Content::Text {
            provider: Some(p), ..
        } = &spec.content
            && !self.tts.has_provider(p)
        {
            return Err(AlertError::Invalid(format!("unknown tts provider {p:?}")));
        }
        // A due time in the past plays now, like a calendar event that is already late.
        let due_at = at.filter(|t| *t > Local::now());

        // Quiet hours only gate the immediate path; a scheduled alert is judged when it fires.
        if due_at.is_none() && priority == AlertPriority::Notification && self.quiet_hours_active()
        {
            return Ok(Outcome::Suppressed {
                id: format!("{namespace}/{name}"),
                reason: "quiet_hours",
            });
        }

        let (old, id, label) = {
            let mut state = self.state.lock().unwrap();
            // An identical apply changes nothing (kubectl-style idempotence).
            if let Some(entry) = state
                .namespaces
                .get(&namespace)
                .and_then(|alerts| alerts.get(&name))
                && entry.spec == spec
                && entry.priority == priority
                && entry.due_at == due_at
            {
                return Ok(Outcome::Unchanged {
                    id: entry
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("{namespace}/{name}")),
                });
            }
            if due_at.is_none() && state.queue.len() >= self.cfg.max_queue {
                return Err(AlertError::QueueFull);
            }
            let old = state
                .namespaces
                .get_mut(&namespace)
                .and_then(|alerts| alerts.remove(&name));
            // "<ns>/<name>#<n>" reads well in logs and stays unique across replaces.
            let id = due_at.is_none().then(|| {
                format!(
                    "{namespace}/{name}#{}",
                    self.named_seq.fetch_add(1, Ordering::Relaxed)
                )
            });
            let label = label_of(&spec.content);
            state
                .namespaces
                .entry(namespace.clone())
                .or_default()
                .insert(
                    name.clone(),
                    StoredEntry {
                        id: id.clone(),
                        priority,
                        label: label.clone(),
                        spec: spec.clone(),
                        due_at,
                        attempts: 0,
                    },
                );
            (old, id, label)
        };
        // Replace: the stored alert with this name is cancelled. If the new render then
        // fails, the old one is already gone — last apply wins.
        if let Some(old) = old
            && let Some(old_id) = old.id
        {
            let _ = self.commands.try_send(Command::CancelAlert(old_id));
        }

        if let Some(due) = due_at {
            tracing::info!(namespace = %namespace, name = %name, due = %due, "alert scheduled");
            return Ok(Outcome::Scheduled {
                id: format!("{namespace}/{name}"),
                due_at: due.to_rfc3339(),
            });
        }

        let id = id.expect("an immediate apply reserved an engine id");
        let rendered = self
            .render(priority, spec.content.clone(), spec.repeat, spec.chime)
            .await;
        let (audio, provider) = match rendered {
            Ok(r) => r,
            Err(err) => {
                self.remove_if_ours(&namespace, &name, &id);
                return Err(err);
            }
        };
        let duration_ms = (audio.len() / audio::CHANNELS) as u64 * 1000 / self.rate as u64;
        match self.commit(&namespace, &name, &id, priority, &label, audio) {
            Ok(()) => {
                tracing::info!(id = %id, ?priority, duration_ms, "alert queued");
                Ok(Outcome::Queued {
                    id,
                    provider,
                    duration_ms,
                })
            }
            // A newer apply owns the entry now; this one never made it out.
            Err(CommitError::Replaced) => Ok(Outcome::Suppressed {
                id,
                reason: "replaced",
            }),
            Err(CommitError::QueueFull) => {
                self.remove_if_ours(&namespace, &name, &id);
                Err(AlertError::QueueFull)
            }
            Err(CommitError::EngineStopped) => {
                self.remove_if_ours(&namespace, &name, &id);
                Err(AlertError::Failed(anyhow!("audio engine stopped")))
            }
        }
    }

    /// Hand a rendered clip to the mixer, unless the entry was replaced while it rendered.
    fn commit(
        &self,
        namespace: &str,
        name: &str,
        id: &str,
        priority: AlertPriority,
        label: &str,
        audio: Vec<f32>,
    ) -> Result<(), CommitError> {
        {
            let mut state = self.state.lock().unwrap();
            let still_ours = state
                .namespaces
                .get(namespace)
                .and_then(|alerts| alerts.get(name))
                .is_some_and(|entry| entry.id.as_deref() == Some(id));
            if !still_ours {
                return Err(CommitError::Replaced);
            }
            if state.queue.len() >= self.cfg.max_queue {
                return Err(CommitError::QueueFull);
            }
            let info = AlertInfo {
                id: id.into(),
                priority,
                label: label.into(),
                namespace: Some(namespace.into()),
                name: Some(name.into()),
            };
            // Track it before the mixer can report it started.
            state.queue.push(info);
        }
        let prepared = PreparedAlert {
            id: id.into(),
            priority,
            audio: Arc::new(audio),
        };
        if let Err(err) = self.commands.try_send(Command::Enqueue(prepared)) {
            let mut state = self.state.lock().unwrap();
            state.queue.retain(|a| a.id != id);
            return Err(match err {
                TrySendError::Full(_) => CommitError::QueueFull,
                TrySendError::Disconnected(_) => CommitError::EngineStopped,
            });
        }
        Ok(())
    }

    /// Everything whose moment has come, oldest first. Each entry is marked as handed off
    /// (engine id reserved) so later ticks, applies and deletes agree on who owns it.
    async fn dispatch_due(&self) {
        let now = Local::now();
        let mut due = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            for (namespace, alerts) in &mut state.namespaces {
                for (name, entry) in alerts {
                    if let Some(due_at) = entry.due_at
                        && due_at <= now
                    {
                        let id = format!(
                            "{namespace}/{name}#{}",
                            self.named_seq.fetch_add(1, Ordering::Relaxed)
                        );
                        entry.id = Some(id.clone());
                        entry.due_at = None;
                        due.push((namespace.clone(), name.clone(), id, due_at, entry.clone()));
                    }
                }
            }
        }
        due.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.1.cmp(&b.1)));
        for (namespace, name, id, due_at, entry) in due {
            self.dispatch_one(&namespace, &name, &id, due_at, &entry)
                .await;
        }
    }

    /// Play one due alert: render it, then enqueue it through the same commit path as an
    /// immediate apply. Failures retry a few times before the alert is dropped.
    async fn dispatch_one(
        &self,
        namespace: &str,
        name: &str,
        id: &str,
        due_at: DateTime<Local>,
        entry: &StoredEntry,
    ) {
        if entry.priority == AlertPriority::Notification && self.quiet_hours_active() {
            if self.remove_if_ours(namespace, name, id).is_some() {
                tracing::info!(id = id, "scheduled alert suppressed by quiet hours");
            }
            return;
        }
        let rendered = self
            .render(
                entry.priority,
                entry.spec.content.clone(),
                entry.spec.repeat,
                entry.spec.chime,
            )
            .await;
        let audio = match rendered {
            Ok((audio, _)) => audio,
            Err(err) => {
                tracing::warn!("scheduled alert {id} failed to render: {}", err.message());
                self.retry_or_drop(namespace, name, id);
                return;
            }
        };
        match self.commit(namespace, name, id, entry.priority, &entry.label, audio) {
            Ok(()) => tracing::info!(id = id, due = %due_at, "scheduled alert queued"),
            // A newer apply owns the entry now; this one never made it out.
            Err(CommitError::Replaced) => {}
            Err(err) => {
                tracing::warn!("scheduled alert {id} could not be queued: {err:?}");
                self.retry_or_drop(namespace, name, id);
            }
        }
    }

    /// After a failed dispatch: try again shortly, or drop the alert with an error.
    fn retry_or_drop(&self, namespace: &str, name: &str, id: &str) {
        let (attempts, drop_it) = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state
                .namespaces
                .get_mut(namespace)
                .and_then(|alerts| alerts.get_mut(name))
            else {
                return;
            };
            if entry.id.as_deref() != Some(id) {
                return; // replaced while we worked on it
            }
            entry.attempts += 1;
            if entry.attempts >= DISPATCH_ATTEMPTS {
                (entry.attempts, true)
            } else {
                entry.due_at = Some(Local::now() + RETRY_AFTER);
                entry.id = None; // back to waiting
                (entry.attempts, false)
            }
        };
        if drop_it {
            let mut state = self.state.lock().unwrap();
            remove_entry(&mut state, namespace, name);
            tracing::error!("scheduled alert {id} dropped after {attempts} attempts");
        } else {
            tracing::warn!("scheduled alert {id} will be retried (attempt {attempts})");
        }
    }

    /// Remove the entry again if it is still the one we reserved; a newer apply owns it
    /// otherwise. Returns what was removed.
    fn remove_if_ours(&self, namespace: &str, name: &str, id: &str) -> Option<StoredEntry> {
        let mut state = self.state.lock().unwrap();
        let ours = state
            .namespaces
            .get(namespace)
            .and_then(|alerts| alerts.get(name))
            .is_some_and(|entry| entry.id.as_deref() == Some(id));
        if ours {
            remove_entry(&mut state, namespace, name)
        } else {
            None
        }
    }

    /// Post an anonymous alert: dedupe, render, enqueue.
    async fn enqueue_alert(
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

        let label = label_of(&content);
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
            namespace: None,
            name: None,
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
            namespaces: namespaces_view(&state),
        }
    }

    /// Every namespace and its alerts, e.g. `calendar`, `agents`, `script`.
    pub fn list_namespaces(&self) -> Vec<NamespaceInfo> {
        let state = self.state.lock().unwrap();
        namespaces_view(&state)
    }

    /// The alerts of one namespace: playing, then pending, then the schedule in time order.
    /// None when it does not exist.
    pub fn namespace_alerts(&self, namespace: &str) -> Option<Vec<StoredAlert>> {
        let state = self.state.lock().unwrap();
        state
            .namespaces
            .get(namespace)
            .map(|alerts| stored_alerts(&state, namespace, alerts))
    }

    /// One stored alert. None when it does not exist.
    pub fn stored_alert(&self, namespace: &str, name: &str) -> Option<StoredAlert> {
        let state = self.state.lock().unwrap();
        state
            .namespaces
            .get(namespace)
            .and_then(|alerts| alerts.get(name))
            .map(|entry| stored_alert(&state, namespace, name, entry))
    }

    /// Delete a stored alert: a playing one stops, a queued one leaves the queue, a
    /// scheduled one simply never happens. False when it does not exist.
    pub fn delete_alert(&self, namespace: &str, name: &str) -> bool {
        let removed = {
            let mut state = self.state.lock().unwrap();
            remove_entry(&mut state, namespace, name)
        };
        match removed {
            Some(entry) => {
                if let Some(id) = entry.id {
                    let _ = self.commands.try_send(Command::CancelAlert(id));
                }
                true
            }
            None => false,
        }
    }

    /// Delete a namespace and every alert in it. False when it does not exist.
    pub fn delete_namespace(&self, namespace: &str) -> bool {
        let removed = {
            let mut state = self.state.lock().unwrap();
            state.namespaces.remove(namespace)
        };
        let Some(entries) = removed else {
            return false;
        };
        for entry in entries.into_values() {
            if let Some(id) = entry.id {
                let _ = self.commands.try_send(Command::CancelAlert(id));
            }
        }
        true
    }

    fn quiet_hours_active(&self) -> bool {
        self.quiet_hours
            .is_some_and(|(start, end)| in_window(Local::now().time(), start, end))
    }
}

/// Handing a rendered clip to the mixer can go wrong in three ways.
#[derive(Debug)]
enum CommitError {
    /// A newer apply owns the entry now.
    Replaced,
    QueueFull,
    EngineStopped,
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

/// Parse an `at` time: RFC 3339 with any offset, or a local "YYYY-MM-DD HH:MM[:SS]"
/// (with 'T' or a space, minutes optional), or a plain local date (midnight).
fn parse_at(value: &str) -> Result<DateTime<Local>, AlertError> {
    let value = value.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Local));
    }
    let naive = if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        date.and_hms_opt(0, 0, 0)
            .ok_or_else(|| AlertError::Invalid("`at` is not a valid date".into()))?
    } else {
        [
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%dT%H:%M",
            "%Y-%m-%d %H:%M",
        ]
        .iter()
        .find_map(|fmt| NaiveDateTime::parse_from_str(value, fmt).ok())
        .ok_or_else(|| {
            AlertError::Invalid(
                "`at` must be RFC 3339 or \"YYYY-MM-DD HH:MM\" in local time".into(),
            )
        })?
    };
    match naive.and_local_timezone(Local) {
        MappedLocalTime::Single(at) => Ok(at),
        // Ambiguous in a DST fall-back gap: take the earlier reading.
        MappedLocalTime::Ambiguous(at, _) => Ok(at),
        MappedLocalTime::None => Err(AlertError::Invalid(
            "`at` falls inside a DST gap; pick a time that exists".into(),
        )),
    }
}

/// The short human-readable form of an alert's content.
fn label_of(content: &Content) -> String {
    match content {
        Content::Text { text, .. } => text.chars().take(120).collect(),
        Content::Url(url) => url.clone(),
        Content::Audio { .. } => "uploaded audio".into(),
        Content::ChimeOnly => "chime".into(),
    }
}

/// Remove an entry and its namespace when it empties. No engine commands.
fn remove_entry(state: &mut State, namespace: &str, name: &str) -> Option<StoredEntry> {
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
}

/// Snapshot of the namespaces for the status and the listing endpoints.
fn namespaces_view(state: &State) -> Vec<NamespaceInfo> {
    state
        .namespaces
        .iter()
        .map(|(name, alerts)| NamespaceInfo {
            name: name.clone(),
            alerts: stored_alerts(state, name, alerts),
        })
        .collect()
}

fn stored_alerts(
    state: &State,
    namespace: &str,
    alerts: &BTreeMap<String, StoredEntry>,
) -> Vec<StoredAlert> {
    let mut entries: Vec<(&String, &StoredEntry)> = alerts.iter().collect();
    entries.sort_by(|(an, ae), (bn, be)| {
        entry_state(state, ae)
            .cmp(&entry_state(state, be))
            .then_with(|| ae.due_at.cmp(&be.due_at))
            .then_with(|| an.cmp(bn))
    });
    entries
        .into_iter()
        .map(|(name, entry)| stored_alert(state, namespace, name, entry))
        .collect()
}

fn stored_alert(state: &State, namespace: &str, name: &str, entry: &StoredEntry) -> StoredAlert {
    StoredAlert {
        namespace: namespace.into(),
        name: name.into(),
        priority: entry.priority,
        label: entry.label.clone(),
        id: entry.id.clone(),
        state: entry_state(state, entry),
        due_at: entry.due_at.as_ref().map(|t| t.to_rfc3339()),
    }
}

/// An entry with an engine id is playing or pending; one without is still scheduled.
fn entry_state(state: &State, entry: &StoredEntry) -> StoredState {
    match entry.id.as_deref() {
        Some(id) if state.current.as_ref().is_some_and(|c| c.id == id) => StoredState::Playing,
        Some(_) => StoredState::Pending,
        None => StoredState::Scheduled,
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

    #[test]
    fn at_accepts_rfc3339_naive_and_date_only() {
        let full = parse_at("2026-11-01T14:30:00").unwrap();
        assert_eq!(full, parse_at("2026-11-01 14:30:00").unwrap());
        assert_eq!(full, parse_at("2026-11-01T14:30").unwrap());
        assert_eq!(full, parse_at("2026-11-01 14:30").unwrap());
        assert_eq!(
            parse_at("2026-11-01").unwrap(),
            parse_at("2026-11-01T00:00:00").unwrap()
        );
        // Any offset converts to local time.
        let utc = parse_at("2026-11-01T12:30:00Z").unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-11-01T12:30:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(utc.timestamp(), expected);
        assert!(parse_at("not a time").is_err());
        assert!(parse_at("2026-13-01").is_err());
        assert!(parse_at("").is_err());
    }

    #[test]
    fn stored_alerts_list_playing_pending_then_schedule() {
        let current = AlertInfo {
            id: "cal/now#0".into(),
            priority: AlertPriority::Notification,
            label: "now".into(),
            namespace: Some("cal".into()),
            name: Some("now".into()),
        };
        let mut state = State {
            current: Some(current),
            ..State::default()
        };
        let spec = AlertSpec {
            content: Content::ChimeOnly,
            repeat: 1,
            chime: true,
        };
        let entry = |id: Option<&str>, due_at: Option<DateTime<Local>>| StoredEntry {
            id: id.map(Into::into),
            priority: AlertPriority::Notification,
            label: "l".into(),
            spec: spec.clone(),
            due_at,
            attempts: 0,
        };
        let mut alerts = BTreeMap::new();
        alerts.insert(
            "late".into(),
            entry(None, Some(Local::now() + chrono::Duration::days(1))),
        );
        alerts.insert("queued".into(), entry(Some("cal/queued#1"), None));
        alerts.insert(
            "early".into(),
            entry(None, Some(Local::now() + chrono::Duration::hours(1))),
        );
        alerts.insert("now".into(), entry(Some("cal/now#0"), None));
        state.namespaces.insert("cal".into(), alerts);

        let view = stored_alerts(&state, "cal", &state.namespaces["cal"]);
        let order: Vec<(&str, StoredState)> =
            view.iter().map(|a| (a.name.as_str(), a.state)).collect();
        assert_eq!(
            order,
            [
                ("now", StoredState::Playing),
                ("queued", StoredState::Pending),
                ("early", StoredState::Scheduled),
                ("late", StoredState::Scheduled),
            ]
        );
        // A scheduled entry has no engine id and shows its due time.
        let early = view.iter().find(|a| a.name == "early").unwrap();
        assert!(early.id.is_none());
        assert!(early.due_at.is_some());
    }
}
