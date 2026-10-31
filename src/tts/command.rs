use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::{Speech, SpeechRequest, TempPath, TtsProvider, run_program};
use crate::config::CommandConfig;

/// Any local program that writes an audio file: Piper, espeak-ng, a custom script...
///
/// Placeholders in `command`: `{output}` (file to write), `{text}`, `{voice}`.
/// Arguments are passed directly, never through a shell.
pub struct CommandTts {
    cfg: CommandConfig,
}

impl CommandTts {
    pub fn new(cfg: CommandConfig) -> Result<Self> {
        if cfg.command.is_empty() {
            bail!("command tts provider needs a non-empty `command`");
        }
        if !cfg.command.iter().any(|a| a.contains("{output}")) {
            bail!("command tts provider must write to {{output}}");
        }
        Ok(Self { cfg })
    }
}

#[async_trait]
impl TtsProvider for CommandTts {
    async fn synthesize(&self, request: &SpeechRequest<'_>) -> Result<Speech> {
        let out = TempPath::new(&self.cfg.extension);
        let output = out.0.display().to_string();
        let voice = request.voice.or(self.cfg.voice.as_deref()).unwrap_or("");
        let expand = |arg: &String| {
            arg.replace("{output}", &output)
                .replace("{voice}", voice)
                .replace("{text}", request.text)
        };
        let program = expand(&self.cfg.command[0]);
        let args: Vec<String> = self.cfg.command[1..].iter().map(expand).collect();
        let stdin = self.cfg.stdin.then_some(request.text);
        run_program(
            &program,
            &args,
            stdin,
            Duration::from_secs(self.cfg.timeout_secs),
        )
        .await?;
        let bytes = tokio::fs::read(&out.0)
            .await
            .with_context(|| format!("{program} did not write {output}"))?;
        Ok(Speech {
            bytes,
            extension: self.cfg.extension.clone(),
        })
    }
}
