use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::{Speech, SpeechRequest, TempPath, TtsProvider, run_program};
use crate::config::SayConfig;

/// macOS built-in speech synthesis.
pub struct Say {
    cfg: SayConfig,
}

impl Say {
    pub fn new(cfg: SayConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl TtsProvider for Say {
    async fn synthesize(&self, request: &SpeechRequest<'_>) -> Result<Speech> {
        let out = TempPath::new("wav");
        let mut args = vec![
            "-o".to_string(),
            out.0.display().to_string(),
            "--file-format=WAVE".into(),
            "--data-format=LEI16@24000".into(),
        ];
        if let Some(voice) = request.voice.or(self.cfg.voice.as_deref()) {
            args.extend(["-v".into(), voice.into()]);
        }
        if let Some(rate) = self.cfg.rate {
            args.extend(["-r".into(), rate.to_string()]);
        }
        // Read the text from stdin so text starting with "-" is never taken as an option.
        args.extend(["-f".into(), "-".into()]);
        run_program("say", &args, Some(request.text), Duration::from_secs(60)).await?;
        Ok(Speech {
            bytes: tokio::fs::read(&out.0).await?,
            extension: "wav".into(),
        })
    }
}
