use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use super::{Speech, SpeechRequest, TtsProvider, api_key, check_response};
use crate::config::OpenAiConfig;

/// Any OpenAI-compatible `POST {base_url}/audio/speech` endpoint.
pub struct OpenAi {
    cfg: OpenAiConfig,
    http: reqwest::Client,
}

impl OpenAi {
    pub fn new(cfg: OpenAiConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }
}

#[async_trait]
impl TtsProvider for OpenAi {
    async fn synthesize(&self, request: &SpeechRequest<'_>) -> Result<Speech> {
        let url = format!("{}/audio/speech", self.cfg.base_url.trim_end_matches('/'));
        let mut body = json!({
            "model": self.cfg.model,
            "input": request.text,
            "voice": request.voice.unwrap_or(&self.cfg.voice),
            "response_format": "wav",
        });
        if let Some(instructions) = &self.cfg.instructions {
            body["instructions"] = json!(instructions);
        }
        if let Some(speed) = self.cfg.speed {
            body["speed"] = json!(speed);
        }

        let mut builder = self
            .http
            .post(url)
            .json(&body)
            .timeout(Duration::from_secs(self.cfg.timeout_secs));
        if let Some(env) = &self.cfg.api_key_env {
            builder = builder.bearer_auth(api_key(env)?);
        }
        let bytes = check_response(builder.send().await?)
            .await?
            .bytes()
            .await?
            .to_vec();
        Ok(Speech {
            bytes,
            extension: "wav".into(),
        })
    }
}
