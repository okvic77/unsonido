use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub output: OutputConfig,
    pub mixer: MixerConfig,
    pub alerts: AlertsConfig,
    pub background: Option<crate::inputs::BackgroundRequest>,
    pub airplay: AirplayConfig,
    pub pipe_outputs: Vec<PipeOutputConfig>,
    pub tts: TtsConfig,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        if let Some(dir) = &mut cfg.tts.cache_dir {
            expand_home(dir);
        }
        if let Some(file) = cfg.background.as_mut().and_then(|bg| bg.file.as_mut()) {
            expand_home(file);
        }
        if let OutputConfig::Wav { path, .. } = &mut cfg.output {
            expand_home(path);
        }
        Ok(cfg)
    }
}

/// Expand a leading `~/` to the home directory.
fn expand_home(path: &mut PathBuf) {
    if let (Ok(rest), Some(home)) = (path.strip_prefix("~"), std::env::var_os("HOME")) {
        *path = PathBuf::from(home).join(rest);
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the HTTP control API listens on. Keep it on localhost unless a token is set.
    pub listen: String,
    /// Optional bearer token required on every request.
    pub token: Option<String>,
    /// Read the token from this environment variable instead of the config file.
    pub token_env: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7700".into(),
            token: None,
            token_env: None,
        }
    }
}

impl ServerConfig {
    pub fn resolved_token(&self) -> Result<Option<String>> {
        if let Some(var) = &self.token_env {
            let token = std::env::var(var).with_context(|| format!("env var {var} is not set"))?;
            return Ok(Some(token));
        }
        Ok(self.token.clone())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum OutputConfig {
    /// A real sound device (CoreAudio on macOS, ALSA on Linux).
    Device {
        /// Substring of the device name; the system default when omitted.
        #[serde(default)]
        name: Option<String>,
    },
    /// Render in real time and discard. Useful on headless servers and for testing.
    Null {
        #[serde(default = "default_rate")]
        sample_rate: u32,
    },
    /// Render in real time into a WAV file.
    Wav {
        path: PathBuf,
        #[serde(default = "default_rate")]
        sample_rate: u32,
    },
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self::Device { name: None }
    }
}

fn default_rate() -> u32 {
    48_000
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MixerConfig {
    /// Master volume, 0.0..=1.0.
    pub master_volume: f32,
    /// How much background/foreground audio is lowered under a notification.
    pub notification_duck_db: f32,
    /// How much background/foreground audio is lowered under a critical alert.
    pub critical_duck_db: f32,
    /// Duck ramp-down time; alerts start once the ramp has finished.
    pub attack_ms: u32,
    /// Duck ramp-up time after the last alert ends.
    pub release_ms: u32,
    /// Silence held after an alert before the bed comes back up.
    pub release_delay_ms: u32,
    /// Master volume is raised to at least this level while a critical alert plays.
    pub critical_min_volume: f32,
    /// Crossfade between the background and foreground layers.
    pub crossfade_ms: u32,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            master_volume: 0.8,
            notification_duck_db: -15.0,
            critical_duck_db: -60.0,
            attack_ms: 250,
            release_ms: 800,
            release_delay_ms: 400,
            critical_min_volume: 0.7,
            crossfade_ms: 1000,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    /// Notifications are dropped inside this window; critical alerts still play.
    pub quiet_hours: Option<QuietHours>,
    /// Alerts with an `id` that was already played within this window are ignored.
    pub dedupe_seconds: u64,
    pub max_queue: usize,
    pub max_repeat: u32,
    /// Speech and uploaded audio are normalized to this RMS level.
    pub target_dbfs: f32,
    /// Maximum size of audio downloaded from a URL or uploaded.
    pub max_audio_bytes: usize,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            quiet_hours: None,
            dedupe_seconds: 600,
            max_queue: 32,
            max_repeat: 5,
            target_dbfs: -18.0,
            max_audio_bytes: 25 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    /// Local time, "HH:MM".
    pub start: String,
    pub end: String,
}

/// Copy of the final mix piped as raw f32le stereo to a program, e.g. ffmpeg -> Icecast.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipeOutputConfig {
    pub name: String,
    /// Program and arguments; `{rate}` is replaced with the sample rate.
    pub command: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AirplayConfig {
    /// Advertise the receivers over Bonjour so devices list them.
    pub advertise: bool,
    /// Name of the first speaker; extra ones are numbered ("Unsonido 2", "Unsonido 3", ...).
    pub name: String,
    /// One idle speaker is always available; when it gets a sender another appears, up to this many.
    pub max_receivers: u16,
    /// How long an extra idle speaker stays before it is removed.
    pub idle_grace_secs: u64,
    /// Port of speaker 1; speaker N uses base_port + N - 1.
    pub base_port: u16,
    /// Audio buffered before playback starts when the sender gives no schedule.
    pub buffer_ms: u32,
    /// Attenuation at the bottom of the sender's volume slider (the top is 0 dB).
    pub volume_range_db: f32,
    /// Pause playing senders (via DACP) during critical alerts and resume them afterwards.
    pub pause_on_critical: bool,
    /// Shift AirPlay audio relative to the sender's schedule. Output device latency is already
    /// compensated automatically; this is only for unusual setups.
    pub audio_delay_ms: i32,
}

impl Default for AirplayConfig {
    fn default() -> Self {
        Self {
            advertise: true,
            name: "Unsonido".into(),
            max_receivers: 6,
            idle_grace_secs: 30,
            base_port: 5100,
            buffer_ms: 400,
            volume_range_db: 45.0,
            pause_on_critical: true,
            audio_delay_ms: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    /// Provider used when a request does not name one.
    pub default: String,
    /// Providers tried in order when the requested one fails.
    pub fallback: Vec<String>,
    /// Directory for cached synthesized speech. Disabled when unset.
    pub cache_dir: Option<PathBuf>,
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("say".to_string(), ProviderConfig::Say(SayConfig::default()));
        Self {
            default: "say".into(),
            fallback: Vec::new(),
            cache_dir: None,
            providers,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    /// macOS `say`.
    Say(SayConfig),
    /// Any local program: Piper, espeak-ng, Coqui, a custom script...
    Command(CommandConfig),
    /// ElevenLabs text-to-speech API.
    Elevenlabs(ElevenLabsConfig),
    /// Any OpenAI-compatible `/audio/speech` endpoint (OpenAI, local servers, proxies).
    Openai(OpenAiConfig),
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct SayConfig {
    pub voice: Option<String>,
    /// Words per minute.
    pub rate: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    /// Program and arguments. Placeholders: `{output}`, `{text}`, `{voice}`.
    pub command: Vec<String>,
    /// Pipe the text to the program's stdin.
    #[serde(default)]
    pub stdin: bool,
    /// Extension of the file the program writes, used as a decoding hint.
    #[serde(default = "default_extension")]
    pub extension: String,
    pub voice: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_extension() -> String {
    "wav".into()
}

fn default_timeout() -> u64 {
    60
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ElevenLabsConfig {
    #[serde(default = "default_elevenlabs_key_env")]
    pub api_key_env: String,
    pub voice_id: String,
    #[serde(default = "default_elevenlabs_model")]
    pub model_id: String,
    #[serde(default = "default_elevenlabs_format")]
    pub output_format: String,
    #[serde(default = "default_elevenlabs_url")]
    pub base_url: String,
    pub stability: Option<f32>,
    pub similarity_boost: Option<f32>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_elevenlabs_key_env() -> String {
    "ELEVENLABS_API_KEY".into()
}

fn default_elevenlabs_model() -> String {
    "eleven_multilingual_v2".into()
}

fn default_elevenlabs_format() -> String {
    "mp3_44100_128".into()
}

fn default_elevenlabs_url() -> String {
    "https://api.elevenlabs.io".into()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OpenAiConfig {
    #[serde(default = "default_openai_url")]
    pub base_url: String,
    /// Unset for local servers that need no key.
    pub api_key_env: Option<String>,
    pub model: String,
    pub voice: String,
    /// Style prompt for models that accept one.
    pub instructions: Option<String>,
    pub speed: Option<f32>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_openai_url() -> String {
    "https://api.openai.com/v1".into()
}
