//! Pluggable text-to-speech. Providers are named instances from the config, so the same
//! backend can appear several times with different voices ("say_es", "say_en", "eleven_alert").

mod command;
mod elevenlabs;
mod openai;
mod say;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::{ProviderConfig, TtsConfig};

pub struct SpeechRequest<'a> {
    pub text: &'a str,
    /// Provider-specific voice override (a `say` voice name, an ElevenLabs voice id...).
    pub voice: Option<&'a str>,
}

/// Encoded audio as returned by a provider.
pub struct Speech {
    pub bytes: Vec<u8>,
    /// File extension used as a decoding hint ("wav", "mp3", ...).
    pub extension: String,
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn synthesize(&self, request: &SpeechRequest<'_>) -> Result<Speech>;
}

struct Entry {
    kind: &'static str,
    provider: Arc<dyn TtsProvider>,
    /// Serialized provider config, so cached audio is invalidated when the config changes.
    fingerprint: String,
}

#[derive(Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub kind: &'static str,
    pub default: bool,
}

pub struct Tts {
    providers: BTreeMap<String, Entry>,
    default: String,
    fallback: Vec<String>,
    cache_dir: Option<PathBuf>,
}

impl Tts {
    pub fn from_config(cfg: &TtsConfig) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent("unsonido").build()?;
        let mut providers = BTreeMap::new();
        for (name, provider_cfg) in &cfg.providers {
            let (kind, provider): (&'static str, Arc<dyn TtsProvider>) = match provider_cfg {
                ProviderConfig::Say(c) => ("say", Arc::new(say::Say::new(c.clone()))),
                ProviderConfig::Command(c) => {
                    ("command", Arc::new(command::CommandTts::new(c.clone())?))
                }
                ProviderConfig::Elevenlabs(c) => (
                    "elevenlabs",
                    Arc::new(elevenlabs::ElevenLabs::new(c.clone(), http.clone())),
                ),
                ProviderConfig::Openai(c) => (
                    "openai",
                    Arc::new(openai::OpenAi::new(c.clone(), http.clone())),
                ),
            };
            let fingerprint = serde_json::to_string(provider_cfg)?;
            providers.insert(
                name.clone(),
                Entry {
                    kind,
                    provider,
                    fingerprint,
                },
            );
        }
        for name in std::iter::once(&cfg.default).chain(&cfg.fallback) {
            if !providers.contains_key(name) {
                bail!("tts provider {name:?} is referenced but not defined in [tts.providers]");
            }
        }
        if let Some(dir) = &cfg.cache_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(Self {
            providers,
            default: cfg.default.clone(),
            fallback: cfg.fallback.clone(),
            cache_dir: cfg.cache_dir.clone(),
        })
    }

    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|(name, e)| ProviderInfo {
                name: name.clone(),
                kind: e.kind,
                default: *name == self.default,
            })
            .collect()
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// Synthesize with the requested provider (or the default), falling back through the
    /// configured chain on failure. Returns the audio and the provider that produced it.
    pub async fn synthesize(
        &self,
        provider: Option<&str>,
        request: &SpeechRequest<'_>,
    ) -> Result<(Speech, String)> {
        let first = provider.unwrap_or(&self.default);
        if !self.providers.contains_key(first) {
            bail!("unknown tts provider {first:?}");
        }
        let mut chain = vec![first];
        for name in &self.fallback {
            if !chain.contains(&name.as_str()) {
                chain.push(name);
            }
        }

        let mut errors = Vec::new();
        for name in chain {
            let entry = &self.providers[name];
            // A voice override only makes sense for the provider it was meant for.
            let voice = if name == first { request.voice } else { None };
            let req = SpeechRequest {
                text: request.text,
                voice,
            };
            match self.synthesize_cached(entry, &req).await {
                Ok(speech) => return Ok((speech, name.to_string())),
                Err(err) => {
                    tracing::warn!(provider = name, "tts failed: {err:#}");
                    errors.push(format!("{name}: {err:#}"));
                }
            }
        }
        Err(anyhow!("all tts providers failed: {}", errors.join("; ")))
    }

    async fn synthesize_cached(
        &self,
        entry: &Entry,
        request: &SpeechRequest<'_>,
    ) -> Result<Speech> {
        let Some(dir) = &self.cache_dir else {
            return entry.provider.synthesize(request).await;
        };
        let mut hasher = Sha256::new();
        for part in [
            entry.fingerprint.as_str(),
            request.voice.unwrap_or(""),
            request.text,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        let key = hex::encode(hasher.finalize());
        if let Some(hit) = find_cached(dir, &key).await {
            return Ok(hit);
        }
        let speech = entry.provider.synthesize(request).await?;
        let path = dir.join(format!("{key}.{}", speech.extension));
        if let Err(err) = tokio::fs::write(&path, &speech.bytes).await {
            tracing::warn!("writing tts cache {}: {err}", path.display());
        }
        Ok(speech)
    }
}

const CACHE_EXTENSIONS: [&str; 6] = ["wav", "mp3", "aiff", "ogg", "flac", "m4a"];

async fn find_cached(dir: &Path, key: &str) -> Option<Speech> {
    for ext in CACHE_EXTENSIONS {
        if let Ok(bytes) = tokio::fs::read(dir.join(format!("{key}.{ext}"))).await {
            return Some(Speech {
                bytes,
                extension: ext.to_string(),
            });
        }
    }
    None
}

/// A unique temporary file path that is removed when dropped.
pub(crate) struct TempPath(pub PathBuf);

impl TempPath {
    pub fn new(extension: &str) -> Self {
        Self(std::env::temp_dir().join(format!("unsonido-{}.{extension}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run a program, optionally feeding `stdin`, and fail with its stderr if it exits non-zero.
pub(crate) async fn run_program(
    program: &str,
    args: &[String],
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).await?;
        drop(pipe);
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{program} timed out after {timeout:?}"))??;
    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Wrap raw little-endian 16-bit PCM in a WAV header.
pub(crate) fn wav_from_pcm16(pcm: &[u8], rate: u32, channels: u16) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let block_align = channels * 2;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Turn a non-success HTTP response into an error that includes the body.
pub(crate) async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    bail!(
        "HTTP {status}: {}",
        body.chars().take(500).collect::<String>()
    )
}

pub(crate) fn api_key(env_var: &str) -> Result<String> {
    std::env::var(env_var).with_context(|| format!("env var {env_var} is not set"))
}
