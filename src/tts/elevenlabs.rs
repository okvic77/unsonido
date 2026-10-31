use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::json;

use super::{Speech, SpeechRequest, TtsProvider, api_key, check_response, wav_from_pcm16};
use crate::config::ElevenLabsConfig;

pub struct ElevenLabs {
    cfg: ElevenLabsConfig,
    http: reqwest::Client,
}

impl ElevenLabs {
    pub fn new(cfg: ElevenLabsConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }
}

#[async_trait]
impl TtsProvider for ElevenLabs {
    async fn synthesize(&self, request: &SpeechRequest<'_>) -> Result<Speech> {
        let key = api_key(&self.cfg.api_key_env)?;
        let voice_id = request.voice.unwrap_or(&self.cfg.voice_id);
        let format = self.cfg.output_format.as_str();
        let url = format!(
            "{}/v1/text-to-speech/{voice_id}?output_format={format}",
            self.cfg.base_url.trim_end_matches('/')
        );
        let mut body = json!({ "text": request.text, "model_id": self.cfg.model_id });
        let mut settings = serde_json::Map::new();
        if let Some(v) = self.cfg.stability {
            settings.insert("stability".into(), json!(v));
        }
        if let Some(v) = self.cfg.similarity_boost {
            settings.insert("similarity_boost".into(), json!(v));
        }
        if !settings.is_empty() {
            body["voice_settings"] = settings.into();
        }

        let response = self
            .http
            .post(url)
            .header("xi-api-key", key)
            .json(&body)
            .timeout(Duration::from_secs(self.cfg.timeout_secs))
            .send()
            .await?;
        let bytes = check_response(response).await?.bytes().await?.to_vec();

        // Formats look like "mp3_44100_128", "pcm_24000", "wav_44100".
        let mut parts = format.split('_');
        match (
            parts.next(),
            parts.next().and_then(|r| r.parse::<u32>().ok()),
        ) {
            (Some("mp3"), _) => Ok(Speech {
                bytes,
                extension: "mp3".into(),
            }),
            (Some("wav"), _) => Ok(Speech {
                bytes,
                extension: "wav".into(),
            }),
            (Some("pcm"), Some(rate)) => Ok(Speech {
                bytes: wav_from_pcm16(&bytes, rate, 1),
                extension: "wav".into(),
            }),
            _ => {
                bail!("unsupported ElevenLabs output_format {format:?}; use mp3_*, wav_* or pcm_*")
            }
        }
    }
}
