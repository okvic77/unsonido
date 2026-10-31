//! The audio side of an AirPlay 1 session: three UDP sockets (audio, control, timing), a
//! reorder buffer with retransmission requests, decryption, decoding, and the stream writer.

use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::packet::Packet;
use symphonia::default::codecs::AlacDecoder;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use super::crypto::PacketCipher;
use crate::stream::StreamWriter;

/// How long a missing packet may hold up playback before it is replaced by silence.
const GAP_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_REORDER: usize = 256;

#[derive(Clone, Debug)]
pub enum AudioFormat {
    Alac {
        cookie: [u8; 24],
        frame_length: u32,
        rate: u32,
    },
    Pcm {
        rate: u32,
        channels: usize,
    },
}

impl AudioFormat {
    /// Parse the SDP `fmtp` line for ALAC:
    /// `96 352 0 16 40 10 14 2 255 0 0 44100`
    /// = payload, frame length, version, bit depth, pb, mb, kb, channels, max run,
    ///   max frame bytes, average bitrate, sample rate.
    pub fn alac_from_fmtp(fmtp: &str) -> Result<Self> {
        let v: Vec<u32> = fmtp
            .split_whitespace()
            .map(|x| x.parse().context("bad fmtp"))
            .collect::<Result<_>>()?;
        if v.len() < 12 {
            return Err(anyhow!("fmtp has {} fields, expected 12", v.len()));
        }
        let mut cookie = Vec::with_capacity(24);
        cookie.extend(v[1].to_be_bytes());
        cookie.extend([
            v[2] as u8, v[3] as u8, v[4] as u8, v[5] as u8, v[6] as u8, v[7] as u8,
        ]);
        cookie.extend((v[8] as u16).to_be_bytes());
        cookie.extend(v[9].to_be_bytes());
        cookie.extend(v[10].to_be_bytes());
        cookie.extend(v[11].to_be_bytes());
        Ok(Self::Alac {
            cookie: cookie.try_into().expect("24 bytes"),
            frame_length: v[1],
            rate: v[11],
        })
    }

    fn rate(&self) -> u32 {
        match self {
            Self::Alac { rate, .. } | Self::Pcm { rate, .. } => *rate,
        }
    }

    fn frames_per_packet(&self) -> usize {
        match self {
            Self::Alac { frame_length, .. } => *frame_length as usize,
            Self::Pcm { .. } => 352,
        }
    }
}

pub enum Msg {
    /// An RTP audio packet, starting at the 12-byte RTP header.
    Packet(Vec<u8>),
    /// Drop queued audio (pause or seek). Packets older than `seq` that arrive late are ignored.
    Flush { seq: Option<u16> },
    /// Sync packet: frame `rtp - latency` plays at sender time `ntp`.
    Sync {
        rtp_less_latency: u32,
        ntp: u64,
        rtp: u32,
    },
}

/// The receiver's own minimum latency, announced as `Audio-Latency` in the RECORD response;
/// senders expect it to be added to the latency in their sync packets.
pub const RECEIVER_LATENCY_FRAMES: u32 = 11_025;

/// Sender-clock minus local-clock, estimated from timing exchanges.
#[derive(Default)]
struct Clock {
    /// Recent (round trip, offset) samples in seconds.
    samples: VecDeque<(f64, f64)>,
}

impl Clock {
    fn add(&mut self, round_trip: f64, offset: f64) {
        if self.samples.len() == 16 {
            self.samples.pop_front();
        }
        self.samples.push_back((round_trip, offset));
    }

    /// The offset from the fastest exchange, which carries the least network delay error.
    fn offset(&self) -> Option<f64> {
        self.samples
            .iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|s| s.1)
    }
}

/// Ports the receiver opened, for the SETUP response.
pub struct LocalPorts {
    pub audio: u16,
    pub control: u16,
    pub timing: u16,
}

/// A running session. Dropping it stops the network tasks and the decoder thread.
pub struct AudioSession {
    tasks: Vec<JoinHandle<()>>,
    packets: Sender<Msg>,
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl AudioSession {
    /// A session with no network side, for pool tests.
    #[cfg(test)]
    pub fn idle_for_tests() -> Self {
        Self {
            tasks: Vec::new(),
            packets: crossbeam_channel::bounded(1).0,
        }
    }

    pub fn flush(&self, seq: Option<u16>) {
        let _ = self.packets.try_send(Msg::Flush { seq });
    }
}

pub struct SessionParams {
    pub format: AudioFormat,
    pub cipher: Option<PacketCipher>,
    pub writer: Arc<Mutex<StreamWriter>>,
    /// Sender address with its control and timing ports.
    pub client_control: SocketAddr,
    pub client_timing: SocketAddr,
    /// Bind address family/interface, taken from the RTSP connection.
    pub bind_ip: std::net::IpAddr,
    pub label: String,
    /// Extra delay added to the sender's schedule, to tune lip sync for the output device.
    pub audio_delay: f64,
}

pub fn start(params: SessionParams) -> Result<(AudioSession, LocalPorts)> {
    let bind = SocketAddr::new(params.bind_ip, 0);
    let open = || -> Result<StdUdpSocket> {
        let socket = StdUdpSocket::bind(bind).with_context(|| format!("binding UDP on {bind}"))?;
        socket.set_nonblocking(true)?;
        Ok(socket)
    };
    let (audio, control, timing) = (open()?, open()?, open()?);
    let ports = LocalPorts {
        audio: audio.local_addr()?.port(),
        control: control.local_addr()?.port(),
        timing: timing.local_addr()?.port(),
    };
    // The decoder thread sends retransmission requests from the control socket.
    let resend_socket = control.try_clone()?;

    let (tx, rx) = crossbeam_channel::bounded::<Msg>(1024);
    let audio = Arc::new(UdpSocket::from_std(audio)?);
    let control = Arc::new(UdpSocket::from_std(control)?);
    let timing = Arc::new(UdpSocket::from_std(timing)?);

    let clock = Arc::new(Mutex::new(Clock::default()));
    let mut tasks = Vec::new();
    {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, _)) = audio.recv_from(&mut buf).await {
                if n > 12 && buf[1] & 0x7f == 0x60 {
                    let _ = tx.try_send(Msg::Packet(buf[..n].to_vec()));
                }
            }
        }));
    }
    {
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, _)) = control.recv_from(&mut buf).await {
                // 0x56: a retransmitted audio packet wrapped in a 4-byte header.
                if n > 16 && buf[1] & 0x7f == 0x56 {
                    let _ = tx.try_send(Msg::Packet(buf[4..n].to_vec()));
                }
                // 0x54: sync, mapping an RTP timestamp to the sender's clock.
                if n >= 20 && buf[1] & 0x7f == 0x54 {
                    let u32_at =
                        |i: usize| u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
                    let ntp = u64::from_be_bytes(buf[8..16].try_into().expect("8 bytes"));
                    let _ = tx.try_send(Msg::Sync {
                        rtp_less_latency: u32_at(4),
                        ntp,
                        rtp: u32_at(16),
                    });
                }
            }
        }));
    }
    {
        let socket = Arc::clone(&timing);
        let clock = Arc::clone(&clock);
        tasks.push(tokio::spawn(async move {
            let mut buf = [0u8; 128];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                // A reply (0x53) to our own timing request: standard NTP offset estimate.
                if n >= 32 && buf[1] & 0x7f == 0x53 {
                    let t4 = ntp_seconds(ntp_now());
                    let at = |i: usize| {
                        ntp_seconds(u64::from_be_bytes(
                            buf[i..i + 8].try_into().expect("8 bytes"),
                        ))
                    };
                    let (t1, t2, t3) = (at(8), at(16), at(24));
                    let round_trip = (t4 - t1) - (t3 - t2);
                    let offset = ((t2 - t1) + (t3 - t4)) / 2.0;
                    if t1 > 0.0 && round_trip >= 0.0 {
                        clock.lock().unwrap().add(round_trip, offset);
                    }
                    continue;
                }
                // Answer timing requests (0x52) like an NTP server would.
                if n >= 32 && buf[1] & 0x7f == 0x52 {
                    let now = ntp_now();
                    let mut reply = [0u8; 32];
                    reply[0] = 0x80;
                    reply[1] = 0xd3;
                    reply[2..4].copy_from_slice(&7u16.to_be_bytes());
                    reply[8..16].copy_from_slice(&buf[24..32]);
                    reply[16..24].copy_from_slice(&now.to_be_bytes());
                    reply[24..32].copy_from_slice(&now.to_be_bytes());
                    let _ = socket.send_to(&reply, from).await;
                }
            }
        }));
    }
    {
        // Senders expect the receiver to ask for the time periodically.
        let socket = Arc::clone(&timing);
        let target = params.client_timing;
        tasks.push(tokio::spawn(async move {
            let mut sent = 0u32;
            loop {
                let mut request = [0u8; 32];
                request[0] = 0x80;
                request[1] = 0xd2;
                request[2..4].copy_from_slice(&7u16.to_be_bytes());
                request[24..32].copy_from_slice(&ntp_now().to_be_bytes());
                let _ = socket.send_to(&request, target).await;
                // A quick burst first so playback can be scheduled within a second.
                sent = sent.saturating_add(1);
                let wait = if sent < 6 { 150 } else { 3000 };
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
        }));
    }

    let thread_name = format!("airplay-{}", params.label);
    let decoder = Decoder::new(params, resend_socket, clock)?;
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || decoder.run(rx))?;

    Ok((AudioSession { tasks, packets: tx }, ports))
}

/// NTP timestamp: seconds since 1900 in the high 32 bits, fraction in the low 32.
fn ntp_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + 2_208_988_800;
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

fn ntp_seconds(ntp: u64) -> f64 {
    (ntp >> 32) as f64 + (ntp & 0xffff_ffff) as f64 / 4_294_967_296.0
}

/// Sync anchor: RTP frame `rtp` plays at sender time `sender_time` (NTP seconds).
#[derive(Clone, Copy)]
struct Anchor {
    rtp: u32,
    sender_time: f64,
}

struct Decoder {
    format: AudioFormat,
    cipher: Option<PacketCipher>,
    alac: Option<AlacDecoder>,
    writer: Arc<Mutex<StreamWriter>>,
    resend_socket: StdUdpSocket,
    client_control: SocketAddr,
    /// Extended (non-wrapping) sequence number of the next packet to play.
    next: Option<u64>,
    /// Decrypted payloads with their RTP timestamps, by extended sequence number.
    pending: BTreeMap<u64, (u32, Vec<u8>)>,
    clock: Arc<Mutex<Clock>>,
    anchor: Option<Anchor>,
    audio_delay: f64,
    /// Clock offsets measured from sync packet arrivals, used until timing replies arrive.
    sync_offsets: VecDeque<f64>,
    warned_bad_schedule: bool,
    logged_schedule: bool,
    gap_since: Option<Instant>,
    drop_before: Option<u16>,
    resend_seq: u16,
    samples: Vec<f32>,
    label: String,
}

impl Decoder {
    fn new(
        params: SessionParams,
        resend_socket: StdUdpSocket,
        clock: Arc<Mutex<Clock>>,
    ) -> Result<Self> {
        let alac = match &params.format {
            AudioFormat::Alac { cookie, rate, .. } => {
                let mut codec = AudioCodecParameters::new();
                codec
                    .for_codec(CODEC_ID_ALAC)
                    .with_sample_rate(*rate)
                    .with_extra_data(cookie.to_vec().into());
                Some(
                    AlacDecoder::try_new(&codec, &AudioDecoderOptions::default())
                        .context("creating ALAC decoder")?,
                )
            }
            AudioFormat::Pcm { .. } => None,
        };
        Ok(Self {
            format: params.format,
            cipher: params.cipher,
            alac,
            writer: params.writer,
            resend_socket,
            client_control: params.client_control,
            next: None,
            pending: BTreeMap::new(),
            clock,
            anchor: None,
            audio_delay: params.audio_delay,
            sync_offsets: VecDeque::new(),
            warned_bad_schedule: false,
            logged_schedule: false,
            gap_since: None,
            drop_before: None,
            resend_seq: 0,
            samples: Vec::new(),
            label: params.label,
        })
    }

    fn run(mut self, rx: Receiver<Msg>) {
        tracing::debug!(receiver = %self.label, "decoder started");
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Packet(packet)) => self.on_packet(packet),
                Ok(Msg::Flush { seq }) => {
                    self.pending.clear();
                    self.next = None;
                    self.gap_since = None;
                    self.drop_before = seq;
                    self.writer.lock().unwrap().flush();
                }
                Ok(Msg::Sync {
                    rtp_less_latency,
                    ntp,
                    rtp,
                }) => {
                    let latency = rtp
                        .wrapping_sub(rtp_less_latency)
                        .wrapping_add(RECEIVER_LATENCY_FRAMES);
                    if self.anchor.is_none() {
                        tracing::debug!(receiver = %self.label, latency_ms = latency as u64 * 1000 / self.format.rate() as u64, "AirPlay sync");
                    }
                    let sender_time = ntp_seconds(ntp);
                    // The sync packet carries the sender's current time, so its arrival also
                    // measures the clock offset (minus a few ms of network delay).
                    self.sync_offsets
                        .push_back(sender_time - ntp_seconds(ntp_now()));
                    if self.sync_offsets.len() > 8 {
                        self.sync_offsets.pop_front();
                    }
                    self.anchor = Some(Anchor {
                        rtp: rtp.wrapping_sub(latency),
                        sender_time,
                    });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.drain();
        }
        self.writer.lock().unwrap().flush();
        tracing::debug!(receiver = %self.label, "decoder stopped");
    }

    fn on_packet(&mut self, mut packet: Vec<u8>) {
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        if let Some(before) = self.drop_before {
            if (seq.wrapping_sub(before) as i16) < 0 {
                return; // Stale audio from before a flush.
            }
            self.drop_before = None;
        }
        let ext = match self.next {
            None => {
                // Start well above zero so wrapping arithmetic never goes negative.
                let ext = (1u64 << 32) + seq as u64;
                self.next = Some(ext);
                ext
            }
            Some(next) => {
                let diff = seq.wrapping_sub(next as u16) as i16;
                if diff < 0 {
                    return; // Late duplicate or already concealed.
                }
                let ext = next + diff as u64;
                if diff as usize > MAX_REORDER * 4 {
                    // A jump this large means the sender restarted its sequence.
                    self.pending.clear();
                    self.next = Some(ext);
                }
                ext
            }
        };
        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let mut payload = packet.split_off(12);
        if let Some(cipher) = &self.cipher {
            cipher.decrypt(&mut payload);
        }
        self.pending.insert(ext, (timestamp, payload));
    }

    fn drain(&mut self) {
        let Some(mut next) = self.next else { return };
        while let Some((timestamp, payload)) = self.pending.remove(&next) {
            self.decode(timestamp, &payload);
            next += 1;
            self.gap_since = None;
        }
        self.next = Some(next);

        let Some((&first, _)) = self.pending.first_key_value() else {
            return;
        };
        // `first > next`: packets are missing in between.
        match self.gap_since {
            None => {
                self.gap_since = Some(Instant::now());
                self.request_resend(next, (first - next).min(u16::MAX as u64) as u16);
            }
            Some(since) if since.elapsed() > GAP_TIMEOUT || self.pending.len() > MAX_REORDER => {
                let missing = (first - next) as usize;
                tracing::debug!(receiver = %self.label, missing, "concealing lost packets");
                let frames = missing.min(MAX_REORDER) * self.format.frames_per_packet();
                let _ = self
                    .writer
                    .lock()
                    .unwrap()
                    .write_silence(frames, self.format.rate());
                self.next = Some(first);
                self.gap_since = None;
                self.drain();
            }
            Some(_) => {}
        }
    }

    fn request_resend(&mut self, first: u64, count: u16) {
        self.resend_seq = self.resend_seq.wrapping_add(1);
        let mut request = [0u8; 8];
        request[0] = 0x80;
        request[1] = 0xd5;
        request[2..4].copy_from_slice(&self.resend_seq.to_be_bytes());
        request[4..6].copy_from_slice(&(first as u16).to_be_bytes());
        request[6..8].copy_from_slice(&count.to_be_bytes());
        let _ = self.resend_socket.send_to(&request, self.client_control);
    }

    fn decode(&mut self, timestamp: u32, payload: &[u8]) {
        let rate = self.format.rate();
        let channels = match (&mut self.alac, &self.format) {
            (Some(alac), _) => {
                let packet = Packet::new(0, 0i64.into(), 352u32.into(), payload.to_vec());
                match alac.decode(&packet) {
                    Ok(buf) => {
                        self.samples.resize(buf.samples_interleaved(), 0.0);
                        buf.copy_to_slice_interleaved(&mut self.samples[..]);
                        buf.spec().channels().count()
                    }
                    Err(err) => {
                        tracing::debug!(receiver = %self.label, "ALAC decode error: {err}");
                        let frames = self.format.frames_per_packet();
                        let _ = self.writer.lock().unwrap().write_silence(frames, rate);
                        return;
                    }
                }
            }
            (None, AudioFormat::Pcm { channels, .. }) => {
                self.samples.clear();
                self.samples.extend(
                    payload
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_be_bytes(*b) as f32 / 32768.0),
                );
                *channels
            }
            (None, AudioFormat::Alac { .. }) => return,
        };
        let frames = (self.samples.len() / channels.max(1)) as u32;
        let due = self.seconds_until_due(timestamp.wrapping_add(frames));
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write(&self.samples, channels, rate, false);
        if let Some(due) = due {
            if !self.logged_schedule {
                self.logged_schedule = true;
                let offset = self.clock.lock().unwrap().offset();
                tracing::info!(
                    receiver = %self.label,
                    due_ms = (due * 1000.0) as i64,
                    timing_replies = offset.is_some(),
                    output_latency_ms = (crate::output::latency() * 1000.0) as i64,
                    "AirPlay playback scheduled to the sender's clock"
                );
            }
            // The newest queued output frame corresponds to input still inside the resampler,
            // and whatever we render now is heard only after the output device's latency.
            let ahead = due - writer.pipeline_delay() - crate::output::latency();
            writer.set_sync_target(
                (ahead * writer.rate() as f64) as i64 * crate::audio::CHANNELS as i64,
            );
        }
    }

    /// Seconds from now until RTP frame `timestamp` should be heard, per the sender's schedule.
    fn seconds_until_due(&mut self, timestamp: u32) -> Option<f64> {
        let anchor = self.anchor?;
        // Sender clocks are arbitrary (not wall time), so an offset is required. Prefer the
        // timing exchange; otherwise use sync arrivals, taking the least delayed one.
        let offset = self
            .clock
            .lock()
            .unwrap()
            .offset()
            .or_else(|| self.sync_offsets.iter().copied().max_by(f64::total_cmp))?;
        let frames_after_anchor = timestamp.wrapping_sub(anchor.rtp) as i32 as f64;
        let local_due =
            anchor.sender_time - offset + frames_after_anchor / self.format.rate() as f64;
        let due = local_due + self.audio_delay - ntp_seconds(ntp_now());
        if !(-5.0..=10.0).contains(&due) {
            // A schedule this far off is a clock problem; buffer normally instead of dropping.
            if !self.warned_bad_schedule {
                self.warned_bad_schedule = true;
                tracing::warn!(receiver = %self.label, due_s = due, "ignoring implausible AirPlay schedule");
            }
            return None;
        }
        Some(due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_alac_fmtp() {
        let format = AudioFormat::alac_from_fmtp("96 352 0 16 40 10 14 2 255 0 0 44100").unwrap();
        let AudioFormat::Alac {
            cookie,
            frame_length,
            rate,
        } = format
        else {
            panic!()
        };
        assert_eq!((frame_length, rate), (352, 44100));
        assert_eq!(&cookie[..6], &[0, 0, 1, 0x60, 0, 16]);
        assert_eq!(&cookie[20..], &44100u32.to_be_bytes());
    }
}
