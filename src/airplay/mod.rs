//! AirPlay 1 (RAOP) receivers, as an elastic pool: one idle speaker is always advertised, and
//! as soon as it gets a sender another one appears ("Unsonido", "Unsonido 2", ...). Idle extras
//! are removed again. Every receiver feeds its own foreground input in the mixer, so several
//! devices can play at once.

mod crypto;
mod dacp;
mod mdns;
mod rtsp;
mod session;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::audio::db_to_gain;
use crate::config::AirplayConfig;
use crate::engine::{Command, Layer};
use crate::stream::{StreamOptions, StreamStatus, StreamWriter, stream};
use session::AudioSession;

pub struct Receiver {
    pub name: String,
    /// Position in the pool, starting at 1; also fixes the port and the advertised id.
    index: u16,
    port: u16,
    mac: [u8; 6],
    volume_range_db: f32,
    audio_delay: f64,
    writer: Arc<Mutex<StreamWriter>>,
    stream: StreamStatus,
    state: Mutex<State>,
    /// Wakes the pool manager when a session starts or ends.
    pool_changed: Arc<Notify>,
    listeners: Mutex<Vec<JoinHandle<()>>>,
    registration: Mutex<Option<mdns::Registration>>,
}

/// What a sender told us about itself: a label for status, and the details needed to
/// remote-control it over DACP (the protocol the Music app and iOS expose for AirPlay remotes).
#[derive(Clone, Default)]
pub(crate) struct SenderInfo {
    pub label: String,
    pub ip: Option<IpAddr>,
    pub dacp_id: Option<String>,
    pub active_remote: Option<String>,
}

#[derive(Clone, Default, Debug, PartialEq, Serialize)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// The RTSP connection that owns the receiver.
struct Session {
    id: u64,
    audio: AudioSession,
    sender: SenderInfo,
    now_playing: Option<NowPlaying>,
    /// Closes the sender's RTSP connection when we disconnect it.
    kick: Arc<Notify>,
}

struct State {
    session: Option<Session>,
    /// The sender's own volume slider, -30..0 dB (-144 = mute).
    volume_db: f32,
    /// Local mute from the control page; the sender is not told.
    muted: bool,
    /// Whether the last volume change from the control page reached the sender.
    volume_synced: Option<bool>,
    /// We paused this sender for a critical alert and owe it a resume.
    paused_for_alert: bool,
    idle_since: Instant,
}

impl State {
    fn owned(&mut self, id: u64) -> Option<&mut Session> {
        self.session.as_mut().filter(|s| s.id == id)
    }
}

#[derive(Serialize)]
pub struct ReceiverStatus {
    pub name: String,
    pub port: u16,
    pub client: Option<String>,
    pub playing: bool,
    pub volume_db: f32,
    /// The sender's volume as a 0..1 slider position, kept in sync both ways.
    pub volume: f32,
    /// Whether the last change from the control page also moved the sender's slider.
    pub volume_synced: Option<bool>,
    pub muted: bool,
    pub now_playing: Option<NowPlaying>,
    /// The sender can be paused and resumed (it sent DACP details).
    pub remote_control: bool,
    pub paused_for_alert: bool,
}

/// Changes from the control page; omitted fields stay as they are.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverUpdate {
    pub volume: Option<f32>,
    pub muted: Option<bool>,
}

impl Receiver {
    fn begin_session(&self, id: u64, audio: AudioSession, sender: SenderInfo, kick: Arc<Notify>) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(old) = &state.session
                && old.id != id
            {
                tracing::info!(receiver = %self.name, from = %old.sender.label, to = %sender.label, "AirPlay session taken over");
                old.kick.notify_one();
            }
            // Dropping the previous session stops its network tasks and decoder.
            state.session = Some(Session {
                id,
                audio,
                sender,
                now_playing: None,
                kick,
            });
            state.volume_db = 0.0;
            state.paused_for_alert = false;
            self.apply_gain(&state);
        }
        self.pool_changed.notify_one();
    }

    fn end_session(&self, id: u64) {
        {
            let mut state = self.state.lock().unwrap();
            if state.owned(id).is_none() {
                return;
            }
            let session = state.session.take().expect("checked above");
            state.paused_for_alert = false;
            state.idle_since = Instant::now();
            tracing::info!(receiver = %self.name, client = %session.sender.label, "AirPlay session ended");
        }
        self.pool_changed.notify_one();
    }

    /// Drop the current sender and close its connection.
    pub fn disconnect(&self) -> bool {
        let session = {
            let mut state = self.state.lock().unwrap();
            state.paused_for_alert = false;
            state.idle_since = Instant::now();
            state.session.take()
        };
        let Some(session) = session else { return false };
        tracing::info!(receiver = %self.name, client = %session.sender.label, "AirPlay sender disconnected");
        session.kick.notify_one();
        self.pool_changed.notify_one();
        true
    }

    fn busy(&self) -> bool {
        self.state.lock().unwrap().session.is_some()
    }

    fn idle_for(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap();
        state.session.is_none().then(|| state.idle_since.elapsed())
    }

    /// Senders may send their DACP details on any request; keep the latest.
    fn update_sender(&self, id: u64, sender: &SenderInfo) {
        if let Some(session) = self.state.lock().unwrap().owned(id) {
            session.sender = sender.clone();
        }
    }

    fn flush(&self, id: u64, seq: Option<u16>) {
        if let Some(session) = self.state.lock().unwrap().owned(id) {
            session.audio.flush(seq);
        }
    }

    /// AirPlay volume is -30.0 (quietest) to 0.0 (full), or -144.0 for mute.
    fn set_volume(&self, id: u64, volume_db: f32) {
        let mut state = self.state.lock().unwrap();
        if state.owned(id).is_none() {
            return;
        }
        state.volume_db = volume_db;
        tracing::info!(receiver = %self.name, volume_db, muted = volume_db <= -30.0, "AirPlay volume");
        self.apply_gain(&state);
    }

    /// Apply a change from the control page. A volume change is applied here at once and
    /// pushed to the sender over DACP, so its own volume slider follows.
    pub async fn update(&self, update: &ReceiverUpdate) {
        let sender = {
            let mut state = self.state.lock().unwrap();
            if let Some(muted) = update.muted {
                state.muted = muted;
            }
            let mut sender = None;
            if let Some(position) = update.volume {
                state.volume_db = airplay_db(position);
                sender = state.session.as_ref().map(|s| s.sender.clone());
                if sender.is_none() {
                    state.volume_synced = None;
                }
            }
            self.apply_gain(&state);
            sender.map(|s| (s, state.volume_db))
        };
        let Some((sender, volume_db)) = sender else {
            return;
        };
        let synced = match dacp::set_volume(&sender, &self.name, volume_db).await {
            Ok(method) => {
                tracing::info!(receiver = %self.name, volume_db, method, "sender volume updated");
                true
            }
            Err(err) => {
                tracing::info!(receiver = %self.name, "sender volume not updated: {err:#}");
                false
            }
        };
        self.state.lock().unwrap().volume_synced = Some(synced);
    }

    fn apply_gain(&self, state: &State) {
        // The sender's slider spans -30..0 dB; stretch it over the configured range.
        let gain = if state.muted || state.volume_db <= -30.0 {
            0.0
        } else {
            db_to_gain(state.volume_db / 30.0 * self.volume_range_db)
        };
        self.writer.lock().unwrap().set_gain(gain);
    }

    fn set_now_playing(&self, id: u64, now_playing: NowPlaying) {
        if let Some(session) = self.state.lock().unwrap().owned(id) {
            tracing::info!(receiver = %self.name, title = ?now_playing.title, artist = ?now_playing.artist, "AirPlay now playing");
            session.now_playing = Some(now_playing);
        }
    }

    fn volume_db(&self) -> f32 {
        self.state.lock().unwrap().volume_db
    }

    pub fn status(&self) -> ReceiverStatus {
        let state = self.state.lock().unwrap();
        let session = state.session.as_ref();
        ReceiverStatus {
            name: self.name.clone(),
            port: self.port,
            client: session.map(|s| s.sender.label.clone()),
            playing: self.stream.active(),
            volume_db: state.volume_db,
            volume: slider_position(state.volume_db),
            volume_synced: state.volume_synced,
            muted: state.muted,
            now_playing: session.and_then(|s| s.now_playing.clone()),
            remote_control: session
                .is_some_and(|s| s.sender.dacp_id.is_some() && s.sender.active_remote.is_some()),
            paused_for_alert: state.paused_for_alert,
        }
    }

    /// Stop advertising, stop listening, drop any session and remove the mixer input.
    fn shutdown(&self, commands: &Sender<Command>) {
        self.registration.lock().unwrap().take();
        for task in self.listeners.lock().unwrap().drain(..) {
            task.abort();
        }
        self.disconnect();
        let _ = commands.try_send(Command::RemoveSource(source_name(&self.name)));
        tracing::info!(receiver = %self.name, "AirPlay receiver removed");
    }

    /// Pause the sender if it is playing and can be remote-controlled.
    async fn pause_for_alert(&self) {
        let sender = {
            let state = self.state.lock().unwrap();
            match &state.session {
                Some(s) if self.stream.active() && !state.paused_for_alert => s.sender.clone(),
                _ => return,
            }
        };
        match dacp::send(&sender, "pause").await {
            Ok(()) => {
                tracing::info!(receiver = %self.name, client = %sender.label, "paused sender for critical alert");
                self.state.lock().unwrap().paused_for_alert = true;
            }
            Err(err) => tracing::debug!(receiver = %self.name, "cannot pause sender: {err:#}"),
        }
    }

    async fn resume_after_alert(&self) {
        let sender = {
            let mut state = self.state.lock().unwrap();
            if !std::mem::take(&mut state.paused_for_alert) {
                return;
            }
            match &state.session {
                Some(s) => s.sender.clone(),
                None => return,
            }
        };
        match dacp::send(&sender, "play").await {
            Ok(()) => {
                tracing::info!(receiver = %self.name, client = %sender.label, "resumed sender")
            }
            Err(err) => tracing::warn!(receiver = %self.name, "cannot resume sender: {err:#}"),
        }
    }
}

/// AirPlay volume (-30..0 dB, -144 = mute) to a 0..1 slider position, as senders draw it.
fn slider_position(volume_db: f32) -> f32 {
    if volume_db <= -30.0 {
        0.0
    } else {
        ((volume_db + 30.0) / 30.0).clamp(0.0, 1.0)
    }
}

/// The inverse of `slider_position`.
fn airplay_db(position: f32) -> f32 {
    if position <= 0.0 {
        -144.0
    } else {
        position.clamp(0.0, 1.0) * 30.0 - 30.0
    }
}

fn source_name(receiver: &str) -> String {
    format!("airplay:{receiver}")
}

/// The pool of AirPlay receivers.
pub struct AirPlay {
    cfg: PoolConfig,
    rate: u32,
    commands: Sender<Command>,
    receivers: Mutex<Vec<Arc<Receiver>>>,
    changed: Arc<Notify>,
}

#[derive(Clone)]
struct PoolConfig {
    name: String,
    base_port: u16,
    max_receivers: u16,
    idle_grace: Duration,
    advertise: bool,
    buffer_ms: u32,
    volume_range_db: f32,
    audio_delay: f64,
}

impl AirPlay {
    pub fn status(&self) -> Vec<ReceiverStatus> {
        self.list().iter().map(|r| r.status()).collect()
    }

    pub fn receiver(&self, name: &str) -> Option<Arc<Receiver>> {
        self.list().into_iter().find(|r| r.name == name)
    }

    fn list(&self) -> Vec<Arc<Receiver>> {
        self.receivers.lock().unwrap().clone()
    }

    /// Keep exactly one idle receiver available: add one when all are busy, and remove idle
    /// extras (highest numbers first) once they have been idle for the grace period.
    async fn rebalance(&self) -> Result<()> {
        let receivers = self.list();
        let idle: Vec<&Arc<Receiver>> = receivers.iter().filter(|r| !r.busy()).collect();
        if idle.is_empty() {
            if receivers.len() >= self.cfg.max_receivers as usize {
                return Ok(());
            }
            let used: Vec<u16> = receivers.iter().map(|r| r.index).collect();
            let index = (1..)
                .find(|i| !used.contains(i))
                .expect("a free index exists");
            let receiver = self.start_receiver(index).await?;
            let mut list = self.receivers.lock().unwrap();
            list.push(receiver);
            list.sort_by_key(|r| r.index);
            return Ok(());
        }
        // The lowest-numbered idle receiver stays; others go once they have rested.
        for extra in idle.iter().skip(1) {
            if extra.idle_for().is_some_and(|d| d >= self.cfg.idle_grace) {
                extra.shutdown(&self.commands);
                self.receivers
                    .lock()
                    .unwrap()
                    .retain(|r| r.index != extra.index);
            }
        }
        Ok(())
    }

    fn spawn_manager(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(err) = this.rebalance().await {
                    tracing::warn!("AirPlay pool: {err:#}");
                }
                tokio::select! {
                    _ = this.changed.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        });
    }

    async fn start_receiver(&self, index: u16) -> Result<Arc<Receiver>> {
        let name = if index == 1 {
            self.cfg.name.clone()
        } else {
            format!("{} {index}", self.cfg.name)
        };
        let port = self.cfg.base_port + index - 1;
        let options = StreamOptions {
            target_ms: self.cfg.buffer_ms,
            capacity_ms: self.cfg.buffer_ms * 4 + 4000,
            live: true,
        };
        let (writer, source) = stream(self.rate, options);
        let status = writer.status();
        let mac = mac_for(&name);
        let listeners =
            bind_listeners(port).with_context(|| format!("AirPlay receiver {name:?}"))?;
        let registration = if self.cfg.advertise {
            let service = format!(
                "{}@{}",
                mac.iter().map(|b| format!("{b:02X}")).collect::<String>(),
                name
            );
            Some(mdns::register(&service, "_raop._tcp", port, &txt_record())?)
        } else {
            None
        };
        self.commands
            .try_send(Command::AddSource {
                name: source_name(&name),
                layer: Layer::Foreground,
                source: Box::new(source),
            })
            .map_err(|_| anyhow!("audio engine is busy"))?;
        let receiver = Arc::new(Receiver {
            name: name.clone(),
            index,
            port,
            mac,
            volume_range_db: self.cfg.volume_range_db,
            audio_delay: self.cfg.audio_delay,
            writer: Arc::new(Mutex::new(writer)),
            stream: status,
            state: Mutex::new(State {
                session: None,
                volume_db: 0.0,
                volume_synced: None,
                muted: false,
                paused_for_alert: false,
                idle_since: Instant::now(),
            }),
            pool_changed: Arc::clone(&self.changed),
            listeners: Mutex::new(Vec::new()),
            registration: Mutex::new(registration),
        });
        let tasks = listeners
            .into_iter()
            .map(|listener| {
                // Weak, so a removed receiver is freed even while its accept loop is winding down.
                let weak = Arc::downgrade(&receiver);
                tokio::spawn(async move {
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let Some(receiver) = weak.upgrade() else {
                                    return;
                                };
                                let _ = stream.set_nodelay(true);
                                tokio::spawn(rtsp::handle(stream, receiver));
                            }
                            Err(err) => tracing::warn!("AirPlay accept: {err}"),
                        }
                    }
                })
            })
            .collect();
        *receiver.listeners.lock().unwrap() = tasks;
        tracing::info!(name = %name, port, "AirPlay receiver ready");
        Ok(receiver)
    }

    /// Pause playing senders while a critical alert is active, and resume them afterwards.
    pub fn pause_during_critical_alerts(self: &Arc<Self>, mut critical: watch::Receiver<bool>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            while critical.changed().await.is_ok() {
                let active = *critical.borrow_and_update();
                for receiver in this.list() {
                    if active {
                        receiver.pause_for_alert().await;
                    } else {
                        receiver.resume_after_alert().await;
                    }
                }
            }
        });
    }
}

pub async fn start(
    cfg: &AirplayConfig,
    rate: u32,
    commands: &Sender<Command>,
) -> Result<Arc<AirPlay>> {
    let pool = Arc::new(AirPlay {
        cfg: PoolConfig {
            name: cfg.name.clone(),
            base_port: cfg.base_port,
            max_receivers: cfg.max_receivers.max(1),
            idle_grace: Duration::from_secs(cfg.idle_grace_secs),
            advertise: cfg.advertise,
            buffer_ms: cfg.buffer_ms,
            volume_range_db: cfg.volume_range_db.abs(),
            audio_delay: cfg.audio_delay_ms as f64 / 1000.0,
        },
        rate,
        commands: commands.clone(),
        receivers: Mutex::new(Vec::new()),
        changed: Arc::new(Notify::new()),
    });
    // Start the first receiver now so configuration errors (a taken port) surface at startup.
    pool.rebalance().await?;
    pool.spawn_manager();
    Ok(pool)
}

/// Listen on IPv4 and, where available, IPv6. Separate sockets keep the address seen by the
/// RTSP handler in its native form, which the Apple-Challenge response depends on.
fn bind_listeners(port: u16) -> Result<Vec<TcpListener>> {
    let mut listeners = vec![listen(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?];
    match listen(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))) {
        Ok(v6) => listeners.push(v6),
        Err(err) => tracing::debug!("no IPv6 listener on port {port}: {err:#}"),
    }
    Ok(listeners)
}

fn listen(addr: SocketAddr) -> Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("binding {addr}"))?;
    socket.listen(16)?;
    Ok(TcpListener::from_std(socket.into())?)
}

/// A stable, locally administered MAC-style id derived from the receiver name.
fn mac_for(name: &str) -> [u8; 6] {
    let hash = Sha256::digest(format!("unsonido:{name}").as_bytes());
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&hash[..6]);
    mac[0] = (mac[0] | 0x02) & 0xfe;
    mac
}

/// TXT record of an AirPlay 1 speaker: ALAC or PCM, optional RSA encryption, no password.
fn txt_record() -> Vec<String> {
    [
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0,1",
        "ek=1",
        "sv=false",
        "da=true",
        "sr=44100",
        "ss=16",
        "pw=false",
        "vn=65537",
        "tp=TCP,UDP",
        "md=0,2",
        "vs=105.1",
        "am=Unsonido",
        "sf=0x4",
        "fv=76400.10",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
