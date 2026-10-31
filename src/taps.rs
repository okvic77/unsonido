//! Pipe outputs: copies of the final mix written as raw f32le stereo to a program's stdin.
//! With ffmpeg this publishes the bus to Icecast, records it, or re-streams it anywhere.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::audio::CHANNELS;
use crate::config::PipeOutputConfig;

/// Create the ring buffer for one pipe output and start its writer thread.
/// The returned producer is handed to the mixer, which fills it without blocking.
pub fn spawn(cfg: &PipeOutputConfig, rate: u32) -> Result<Producer<f32>> {
    if cfg.command.is_empty() {
        return Err(anyhow!("pipe output {:?} has an empty command", cfg.name));
    }
    let (producer, consumer) = RingBuffer::new(rate as usize * 2 * CHANNELS);
    let command: Vec<String> = cfg
        .command
        .iter()
        .map(|a| a.replace("{rate}", &rate.to_string()))
        .collect();
    let name = cfg.name.clone();
    std::thread::Builder::new()
        .name(format!("pipe-{name}"))
        .spawn(move || run(&name, &command, consumer))?;
    Ok(producer)
}

fn run(name: &str, command: &[String], mut consumer: Consumer<f32>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match start(command) {
            Ok((child, stdin)) => {
                tracing::info!(output = name, "pipe output started");
                let started = std::time::Instant::now();
                let err = pump(&mut consumer, stdin);
                stop(child);
                tracing::warn!(output = name, "pipe output stopped: {err:#}");
                if started.elapsed() > Duration::from_secs(30) {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(err) => tracing::warn!(output = name, "pipe output failed to start: {err:#}"),
        }
        // Discard audio produced while the program was down, then retry.
        std::thread::sleep(backoff);
        let stale = consumer.slots();
        if let Ok(chunk) = consumer.read_chunk(stale) {
            chunk.commit_all();
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn start(command: &[String]) -> Result<(Child, ChildStdin)> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", command[0]))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    Ok((child, stdin))
}

/// Move samples from the ring to the program until writing fails.
fn pump(consumer: &mut Consumer<f32>, mut stdin: ChildStdin) -> anyhow::Error {
    let mut bytes = Vec::with_capacity(4096 * 4);
    loop {
        let n = consumer.slots().min(4096);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let Ok(chunk) = consumer.read_chunk(n) else {
            continue;
        };
        bytes.clear();
        let (a, b) = chunk.as_slices();
        for s in a.iter().chain(b) {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        chunk.commit_all();
        if let Err(err) = stdin.write_all(&bytes) {
            return err.into();
        }
    }
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}
