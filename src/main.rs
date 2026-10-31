mod airplay;
mod alerts;
mod api;
mod audio;
mod config;
mod engine;
mod inputs;
mod output;
mod stream;
mod taps;
mod tts;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait};
use tracing_subscriber::EnvFilter;

use crate::alerts::AlertService;
use crate::config::Config;
use crate::engine::{Levels, Mixer};
use crate::inputs::Background;
use crate::tts::Tts;

#[derive(Parser)]
#[command(
    version,
    about = "Audio bus that merges sources and alerts into one output"
)]
struct Cli {
    /// Path to the TOML config. Built-in defaults are used when omitted.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List audio output devices.
    Devices,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("unsonido=info")),
        )
        .init();
    let cli = Cli::parse();
    if let Some(Cmd::Devices) = cli.command {
        return list_devices();
    }
    let cfg = Config::load(cli.config.as_deref())?;
    run(cfg).await
}

async fn run(cfg: Config) -> Result<()> {
    let tts = Arc::new(Tts::from_config(&cfg.tts)?);
    let token = cfg.server.resolved_token()?;

    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(256);
    let (ev_tx, ev_rx) = crossbeam_channel::bounded(1024);
    let levels = Arc::new(Levels::default());

    let mixer_levels = Arc::clone(&levels);
    let mixer_cfg = cfg.mixer;
    let master_volume = mixer_cfg.master_volume;
    let pipe_outputs = cfg.pipe_outputs;
    let rate = output::spawn(&cfg.output, move |rate| {
        let mut mixer = Mixer::new(rate, &mixer_cfg, cmd_rx, ev_tx, mixer_levels);
        for pipe in &pipe_outputs {
            match taps::spawn(pipe, rate) {
                Ok(tap) => mixer.add_tap(tap),
                Err(err) => tracing::error!("pipe output {:?}: {err:#}", pipe.name),
            }
        }
        mixer
    })?;

    let background = Arc::new(Background::new(rate, cmd_tx.clone()));
    if let Some(request) = cfg.background
        && let Err(err) = background.play(request)
    {
        tracing::error!("background not started: {err:#}");
    }
    let airplay = airplay::start(&cfg.airplay, rate, &cmd_tx).await?;
    let (critical_tx, critical_rx) = tokio::sync::watch::channel(false);
    if cfg.airplay.pause_on_critical {
        airplay.pause_during_critical_alerts(critical_rx);
    }

    let max_body = cfg.alerts.max_audio_bytes;
    let alerts = AlertService::new(
        cfg.alerts,
        rate,
        master_volume,
        tts,
        cmd_tx,
        levels,
        critical_tx,
    )?;
    alerts.spawn_event_pump(ev_rx);

    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.listen))?;
    let addr = listener.local_addr()?;
    if token.is_none() && !addr.ip().is_loopback() {
        tracing::warn!(
            "API is reachable from the network without a token; set server.token or server.token_env"
        );
    }
    tracing::info!("control API on http://{addr}");
    axum::serve(
        listener,
        api::router(
            api::Services {
                alerts,
                background,
                airplay,
            },
            token,
            max_body,
        ),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string());
    for device in host.output_devices()? {
        let Ok(desc) = device.description() else {
            continue;
        };
        let config = device
            .default_output_config()
            .map(|c| format!("{} Hz, {} ch", c.sample_rate(), c.channels()));
        let marker = if default.as_deref() == Some(desc.name()) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {}  ({})",
            desc.name(),
            config.unwrap_or_else(|e| e.to_string())
        );
    }
    Ok(())
}
