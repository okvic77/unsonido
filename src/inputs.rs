//! Background inputs: local files (looped) and HTTP/Icecast streams, decoded on their own
//! thread and fed to the mixer through a [`StreamWriter`].

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;

use crate::audio::db_to_gain;
use crate::engine::{Command, Layer};
use crate::stream::{StreamOptions, StreamStatus, StreamWriter, WriteError, stream};

pub const BACKGROUND: &str = "background";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundRequest {
    /// http(s) stream URL, e.g. an Icecast mount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// More stream URLs, tried in order when the ones before them fail. With `url` set, it goes
    /// first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Local file, looped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default = "default_gain_db")]
    pub gain_db: f32,
}

pub fn default_gain_db() -> f32 {
    -6.0
}

#[derive(Serialize, Clone)]
pub struct BackgroundStatus {
    #[serde(flatten)]
    pub request: BackgroundRequest,
    pub playing: bool,
    pub title: Option<String>,
    pub state: String,
    /// The stream URL playing now, when several are configured.
    pub current_url: Option<String>,
}

struct Current {
    request: BackgroundRequest,
    stream: StreamStatus,
    info: Arc<Mutex<Info>>,
}

#[derive(Default)]
struct Info {
    title: Option<String>,
    state: String,
    current_url: Option<String>,
}

/// Owns the background slot of the mixer.
pub struct Background {
    rate: u32,
    commands: Sender<Command>,
    current: Mutex<Option<Current>>,
}

impl Background {
    pub fn new(rate: u32, commands: Sender<Command>) -> Self {
        Self {
            rate,
            commands,
            current: Mutex::new(None),
        }
    }

    pub fn play(&self, request: BackgroundRequest) -> Result<()> {
        let urls: Vec<String> = request.url.iter().chain(&request.urls).cloned().collect();
        let location = match (urls.is_empty(), &request.file) {
            (false, None) => {
                for url in &urls {
                    let parsed =
                        reqwest::Url::parse(url).with_context(|| format!("invalid url {url:?}"))?;
                    if !matches!(parsed.scheme(), "http" | "https") {
                        return Err(anyhow!("only http and https urls are supported: {url}"));
                    }
                }
                Location::Urls(urls)
            }
            (true, Some(file)) => {
                if !file.is_file() {
                    return Err(anyhow!("{} is not a file", file.display()));
                }
                Location::File(file.clone())
            }
            _ => return Err(anyhow!("give stream `url`/`urls` or a `file`, not both")),
        };
        let live = matches!(location, Location::Urls(_));
        let options = if live {
            StreamOptions {
                target_ms: 1500,
                capacity_ms: 8000,
                live: true,
            }
        } else {
            StreamOptions {
                target_ms: 200,
                capacity_ms: 2000,
                live: false,
            }
        };
        let (writer, source) = stream(self.rate, options);
        writer.set_gain(db_to_gain(request.gain_db));
        let info = Arc::new(Mutex::new(Info {
            title: None,
            state: "connecting".into(),
            current_url: None,
        }));
        let status = writer.status();
        spawn_decoder(location, writer, Arc::clone(&info))?;
        self.commands
            .try_send(Command::AddSource {
                name: BACKGROUND.into(),
                layer: Layer::Background,
                source: Box::new(source),
            })
            .map_err(|_| anyhow!("audio engine is busy"))?;
        tracing::info!(urls = ?request.url.iter().chain(&request.urls).collect::<Vec<_>>(), file = ?request.file, "background started");
        *self.current.lock().unwrap() = Some(Current {
            request,
            stream: status,
            info,
        });
        Ok(())
    }

    pub fn stop(&self) {
        // Removing the source closes its stream, which ends the decoder thread.
        let _ = self
            .commands
            .try_send(Command::RemoveSource(BACKGROUND.into()));
        *self.current.lock().unwrap() = None;
    }

    pub fn status(&self) -> Option<BackgroundStatus> {
        let current = self.current.lock().unwrap();
        let current = current.as_ref()?;
        let info = current.info.lock().unwrap();
        Some(BackgroundStatus {
            request: current.request.clone(),
            playing: current.stream.active(),
            title: info.title.clone(),
            state: info.state.clone(),
            current_url: info.current_url.clone(),
        })
    }
}

enum Location {
    File(PathBuf),
    /// Stream URLs in order of preference.
    Urls(Vec<String>),
}

/// A stream that played at least this long counts as having worked: when it drops, we start
/// again from the first (preferred) URL instead of moving down the list.
const PLAYED_LONG_ENOUGH: Duration = Duration::from_secs(10);

fn spawn_decoder(
    location: Location,
    mut writer: StreamWriter,
    info: Arc<Mutex<Info>>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("background-decoder".into())
        .spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                let worked = match &location {
                    Location::File(path) => match play_file(path, &mut writer, &info) {
                        Ok(()) => continue, // Loop the file.
                        Err(err) => {
                            tracing::warn!("background file: {err:#}");
                            false
                        }
                    },
                    Location::Urls(urls) => play_urls(urls, &mut writer, &info),
                };
                if writer.is_closed() {
                    return;
                }
                if worked {
                    backoff = Duration::from_secs(1);
                }
                set_state(&info, &format!("retrying in {}s", backoff.as_secs()));
                // Sleep in small steps so removal is noticed quickly.
                let deadline = std::time::Instant::now() + backoff;
                while std::time::Instant::now() < deadline {
                    if writer.is_closed() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !worked {
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        })?;
    Ok(())
}

/// Try each URL in order until one plays. Returns true once a stream played for a while and
/// then stopped (so the caller restarts from the top quickly), false if every URL failed.
fn play_urls(urls: &[String], writer: &mut StreamWriter, info: &Arc<Mutex<Info>>) -> bool {
    for (index, url) in urls.iter().enumerate() {
        {
            let mut info = info.lock().unwrap();
            info.current_url = Some(url.clone());
            info.title = None;
            info.state = "connecting".into();
        }
        let started = std::time::Instant::now();
        let result = play_url(url, writer, info);
        if writer.is_closed() {
            return true;
        }
        match &result {
            Ok(()) => tracing::warn!(%url, "background stream ended"),
            Err(err) => tracing::warn!(%url, "background stream failed: {err:#}"),
        }
        if started.elapsed() >= PLAYED_LONG_ENOUGH {
            return true;
        }
        if let Some(next) = urls.get(index + 1) {
            tracing::info!(%next, "trying fallback stream");
        }
    }
    false
}

fn set_state(info: &Mutex<Info>, state: &str) {
    info.lock().unwrap().state = state.to_string();
}

fn play_file(path: &PathBuf, writer: &mut StreamWriter, info: &Mutex<Info>) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    set_state(info, "playing");
    decode_into(Box::new(file), ext.as_deref(), writer, true)
}

fn play_url(url: &str, writer: &mut StreamWriter, info: &Arc<Mutex<Info>>) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("unsonido")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client
        .get(url)
        .header("Icy-MetaData", "1")
        .send()
        .context("connecting")?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {}", response.status()));
    }
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let metaint = header("icy-metaint").and_then(|v| v.parse::<usize>().ok());
    let hint = header("content-type").and_then(|ct| extension_for_content_type(&ct));
    if let Some(name) = header("icy-name") {
        tracing::info!(station = %name, "connected to stream");
    }
    set_state(info, "playing");

    let reader: Box<dyn Read + Send + Sync> = match metaint {
        Some(interval) if interval > 0 => Box::new(IcyReader {
            inner: response,
            interval,
            until_meta: interval,
            info: Arc::clone(info),
        }),
        _ => Box::new(response),
    };
    let source = ReadOnlySource::new(reader);
    let hint = hint.or_else(|| {
        url.rsplit('.')
            .next()
            .filter(|e| e.len() <= 4)
            .map(str::to_string)
    });
    decode_into(Box::new(source), hint.as_deref(), writer, true)
}

fn extension_for_content_type(content_type: &str) -> Option<String> {
    let ext = match content_type.split(';').next()?.trim() {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "aac",
        "audio/ogg" | "application/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        _ => return None,
    };
    Some(ext.to_string())
}

/// Decode a whole media source into the writer. Returns when the source ends or the stream closes.
fn decode_into(
    source: Box<dyn MediaSource>,
    ext: Option<&str>,
    writer: &mut StreamWriter,
    block: bool,
) -> Result<()> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unrecognized audio format")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("no codec params"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .context("unsupported codec")?;

    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(SymphoniaError::ResetRequired) => return Ok(()),
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(err) => return Err(err).context("reading stream"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(err).context("decoding"),
        };
        let rate = buf.spec().rate();
        let channels = buf.spec().channels().count();
        samples.resize(buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut samples[..]);
        if let Err(WriteError::Closed) = writer.write(&samples, channels, rate, block) {
            return Ok(());
        }
    }
}

/// Strips Icecast/SHOUTcast metadata blocks from the byte stream and records the title.
struct IcyReader<R> {
    inner: R,
    interval: usize,
    until_meta: usize,
    info: Arc<Mutex<Info>>,
}

impl<R: Read> Read for IcyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.until_meta == 0 {
            let mut len = [0u8; 1];
            self.inner.read_exact(&mut len)?;
            let mut meta = vec![0u8; len[0] as usize * 16];
            self.inner.read_exact(&mut meta)?;
            if let Some(title) = parse_stream_title(&meta) {
                tracing::info!(%title, "now playing");
                self.info.lock().unwrap().title = Some(title);
            }
            self.until_meta = self.interval;
        }
        let max = buf.len().min(self.until_meta);
        let n = self.inner.read(&mut buf[..max])?;
        self.until_meta -= n;
        Ok(n)
    }
}

fn parse_stream_title(meta: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(meta);
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let end = text[start..].find("';").map(|e| start + e)?;
    let title = text[start..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icy_reader_strips_metadata() {
        let meta = b"StreamTitle='Artist - Song';";
        let mut block = vec![(meta.len().div_ceil(16)) as u8];
        let mut padded = meta.to_vec();
        padded.resize(block[0] as usize * 16, 0);
        block.extend(padded);

        let mut data = b"abcd".to_vec();
        data.extend(&block);
        data.extend(b"efgh");
        data.push(0); // empty metadata block
        data.extend(b"ij");

        let info = Arc::new(Mutex::new(Info::default()));
        let mut reader = IcyReader {
            inner: &data[..],
            interval: 4,
            until_meta: 4,
            info: Arc::clone(&info),
        };
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcdefghij");
        assert_eq!(info.lock().unwrap().title.as_deref(), Some("Artist - Song"));
    }
}
