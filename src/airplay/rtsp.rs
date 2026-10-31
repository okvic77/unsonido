//! The RTSP control connection of an AirPlay 1 sender.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use super::crypto::{self, PacketCipher};
use super::session::{self, AudioFormat, SessionParams};
use super::{NowPlaying, Receiver};

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
const MAX_BODY: usize = 4 * 1024 * 1024;

struct Request {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn ok() -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn status(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            ..Self::ok()
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }
}

/// State negotiated over one RTSP connection.
struct Connection {
    id: u64,
    receiver: Arc<Receiver>,
    peer: SocketAddr,
    local: SocketAddr,
    user_agent: Option<String>,
    dacp_id: Option<String>,
    active_remote: Option<String>,
    format: Option<AudioFormat>,
    key_iv: Option<([u8; 16], [u8; 16])>,
    /// Notified to close this connection (disconnect from the control page, or takeover).
    kick: Arc<Notify>,
}

pub async fn handle(stream: TcpStream, receiver: Arc<Receiver>) {
    let (peer, local) = match (stream.peer_addr(), stream.local_addr()) {
        (Ok(p), Ok(l)) => (p, l),
        _ => return,
    };
    let mut conn = Connection {
        id: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
        receiver,
        peer,
        local,
        user_agent: None,
        dacp_id: None,
        active_remote: None,
        format: None,
        key_iv: None,
        kick: Arc::new(Notify::new()),
    };
    tracing::debug!(receiver = %conn.receiver.name, %peer, "connection opened");
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let kick = Arc::clone(&conn.kick);
        let read = tokio::select! {
            read = read_request(&mut reader) => read,
            _ = kick.notified() => break,
        };
        let request = match read {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(%peer, "bad RTSP request: {err:#}");
                break;
            }
        };
        let cseq = request.header("CSeq").unwrap_or("0").to_string();
        let mut response = conn.dispatch(&request).await;
        if let Some(challenge) = request.header("Apple-Challenge") {
            match crypto::apple_response(challenge, conn.local.ip(), conn.receiver.mac) {
                Ok(answer) => response = response.header("Apple-Response", answer),
                Err(err) => tracing::warn!("Apple-Challenge: {err:#}"),
            }
        }
        tracing::debug!(receiver = %conn.receiver.name, method = %request.method, uri = %request.uri, status = response.status, headers = ?request.headers, "rtsp");
        if write_response(&mut write, &cseq, response).await.is_err() {
            break;
        }
    }
    conn.receiver.end_session(conn.id);
    tracing::debug!(receiver = %conn.receiver.name, %peer, "connection closed");
}

impl Connection {
    async fn dispatch(&mut self, req: &Request) -> Response {
        let mut sender_changed = false;
        for (header, field) in [
            ("User-Agent", &mut self.user_agent),
            ("DACP-ID", &mut self.dacp_id),
            ("Active-Remote", &mut self.active_remote),
        ] {
            if let Some(value) = req.header(header)
                && field.as_deref() != Some(value)
            {
                *field = Some(value.to_string());
                sender_changed = true;
            }
        }
        if sender_changed {
            self.receiver.update_sender(self.id, &self.sender_info());
        }
        let result = match req.method.as_str() {
            "OPTIONS" => Ok(Response::ok().header(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
            )),
            "ANNOUNCE" => self.announce(req),
            "SETUP" => self.setup(req),
            "RECORD" => {
                tracing::info!(receiver = %self.receiver.name, client = %self.client_label(), "AirPlay streaming");
                Ok(Response::ok().header("Audio-Latency", session::RECEIVER_LATENCY_FRAMES.to_string()))
            }
            "FLUSH" => {
                let seq = req.header("RTP-Info").and_then(|info| rtp_info_field(info, "seq"));
                self.receiver.flush(self.id, seq.and_then(|s| s.parse().ok()));
                Ok(Response::ok())
            }
            "TEARDOWN" => {
                self.receiver.end_session(self.id);
                Ok(Response::ok().header("Connection", "close"))
            }
            "SET_PARAMETER" => {
                self.set_parameter(req);
                Ok(Response::ok())
            }
            "GET_PARAMETER" => {
                let body = String::from_utf8_lossy(&req.body);
                let mut response = Response::ok();
                if body.contains("volume") {
                    response = response.header("Content-Type", "text/parameters");
                    response.body = format!("volume: {:.6}\r\n", self.receiver.volume_db()).into_bytes();
                }
                Ok(response)
            }
            // AirPlay 2 endpoints some senders probe; we only speak AirPlay 1.
            "POST" if req.uri == "/feedback" => Ok(Response::ok()),
            _ => {
                tracing::debug!(method = %req.method, uri = %req.uri, "unsupported RTSP request");
                Ok(Response::status(501, "Not Implemented"))
            }
        };
        result.unwrap_or_else(|err: anyhow::Error| {
            tracing::warn!(receiver = %self.receiver.name, method = %req.method, "{err:#}");
            Response::status(400, "Bad Request")
        })
    }

    fn sender_info(&self) -> super::SenderInfo {
        super::SenderInfo {
            label: self.client_label(),
            ip: Some(canonical(self.peer.ip())),
            dacp_id: self.dacp_id.clone(),
            active_remote: self.active_remote.clone(),
        }
    }

    fn client_label(&self) -> String {
        match &self.user_agent {
            Some(agent) => format!("{} ({agent})", self.peer.ip()),
            None => self.peer.ip().to_string(),
        }
    }

    fn announce(&mut self, req: &Request) -> Result<Response> {
        let sdp = String::from_utf8_lossy(&req.body);
        let mut rtpmap = None;
        let mut fmtp = None;
        let mut aes_key = None;
        let mut aes_iv = None;
        for line in sdp.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("a=rtpmap:") {
                rtpmap = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("a=fmtp:") {
                fmtp = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("a=rsaaeskey:") {
                aes_key = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("a=aesiv:") {
                aes_iv = Some(v.to_string());
            }
        }
        let rtpmap = rtpmap.ok_or_else(|| anyhow!("SDP has no rtpmap"))?;
        let format = if rtpmap.contains("AppleLossless") {
            AudioFormat::alac_from_fmtp(fmtp.as_deref().ok_or_else(|| anyhow!("SDP has no fmtp"))?)?
        } else if let Some(params) = rtpmap
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.strip_prefix("L16/"))
        {
            let mut parts = params.split('/');
            let rate = parts.next().and_then(|r| r.parse().ok()).unwrap_or(44_100);
            let channels = parts.next().and_then(|c| c.parse().ok()).unwrap_or(2);
            AudioFormat::Pcm { rate, channels }
        } else {
            return Ok(Response::status(415, "Unsupported Media Type"));
        };
        self.key_iv = match (aes_key, aes_iv) {
            (Some(key), Some(iv)) => {
                let key = crypto::decrypt_session_key(&key)?;
                let iv: [u8; 16] = crypto::b64_decode(&iv)?
                    .try_into()
                    .map_err(|_| anyhow!("bad aesiv length"))?;
                Some((key, iv))
            }
            _ => None,
        };
        tracing::debug!(receiver = %self.receiver.name, ?format, encrypted = self.key_iv.is_some(), "ANNOUNCE");
        self.format = Some(format);
        Ok(Response::ok())
    }

    fn setup(&mut self, req: &Request) -> Result<Response> {
        let transport = req
            .header("Transport")
            .ok_or_else(|| anyhow!("SETUP without Transport"))?;
        let port = |name: &str| -> Result<u16> {
            transport_field(transport, name)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow!("Transport has no {name}"))
        };
        let (control_port, timing_port) = (port("control_port")?, port("timing_port")?);
        let format = self
            .format
            .clone()
            .ok_or_else(|| anyhow!("SETUP before ANNOUNCE"))?;

        // Talk to the sender on the same address family it connected with.

        let bind_ip = match canonical(self.local.ip()) {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        };
        let params = SessionParams {
            format,
            cipher: self.key_iv.map(|(key, iv)| PacketCipher::new(key, iv)),
            writer: Arc::clone(&self.receiver.writer),
            client_control: sender_addr(self.peer, control_port),
            client_timing: sender_addr(self.peer, timing_port),
            bind_ip,
            label: self.receiver.name.clone(),
            audio_delay: self.receiver.audio_delay,
        };
        let (session, ports) = session::start(params)?;
        self.receiver
            .begin_session(self.id, session, self.sender_info(), Arc::clone(&self.kick));

        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={};server_port={}",
            ports.control, ports.timing, ports.audio
        );
        Ok(Response::ok()
            .header("Transport", transport)
            .header("Session", "1")
            .header("Audio-Jack-Status", "connected; type=analog"))
    }

    fn set_parameter(&mut self, req: &Request) {
        let content_type = req.header("Content-Type").unwrap_or("");
        if content_type.starts_with("application/x-dmap-tagged") {
            self.receiver
                .set_now_playing(self.id, parse_dmap(&req.body));
            return;
        }
        if !content_type.starts_with("text/parameters") {
            return; // Artwork and progress are accepted and ignored.
        }
        for line in String::from_utf8_lossy(&req.body).lines() {
            if let Some(volume) = line
                .strip_prefix("volume:")
                .and_then(|v| v.trim().parse::<f32>().ok())
            {
                self.receiver.set_volume(self.id, volume);
            }
        }
    }
}

/// Track metadata in DMAP: `tag(4) length(4, big endian) value`, with `mlit` as a container.
fn parse_dmap(body: &[u8]) -> NowPlaying {
    fn walk(mut data: &[u8], out: &mut NowPlaying) {
        while data.len() >= 8 {
            let len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
            let Some(value) = data.get(8..8 + len) else {
                return;
            };
            let text =
                || Some(String::from_utf8_lossy(value).into_owned()).filter(|t| !t.is_empty());
            match &data[..4] {
                b"mlit" => walk(value, out),
                b"minm" => out.title = text(),
                b"asar" => out.artist = text(),
                b"asal" => out.album = text(),
                _ => {}
            }
            data = &data[8 + len..];
        }
    }
    let mut out = NowPlaying::default();
    walk(body, &mut out);
    out
}

/// The sender's address with another port. IPv6 link-local addresses keep their interface
/// (scope) id, without which replies to `fe80::` senders never leave this machine.
fn sender_addr(peer: SocketAddr, port: u16) -> SocketAddr {
    match peer {
        SocketAddr::V6(v6) if v6.ip().to_ipv4_mapped().is_some() => {
            SocketAddr::new(canonical(peer.ip()), port)
        }
        mut addr => {
            addr.set_port(port);
            addr
        }
    }
}

/// Map IPv4-mapped IPv6 addresses back to IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub(super) fn transport_field<'a>(transport: &'a str, name: &str) -> Option<&'a str> {
    transport
        .split(';')
        .find_map(|part| part.trim().strip_prefix(name)?.strip_prefix('='))
}

fn rtp_info_field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.split(';')
        .find_map(|part| part.trim().strip_prefix(name)?.strip_prefix('='))
}

async fn read_request<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Request>> {
    let mut line = String::new();
    // Skip blank lines between requests.
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("empty request line"))?
        .to_string();
    let uri = parts.next().unwrap_or("*").to_string();

    let mut headers = HashMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(anyhow!("body of {length} bytes is too large"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.context("reading body")?;
    Ok(Some(Request {
        method,
        uri,
        headers,
        body,
    }))
}

async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    cseq: &str,
    response: Response,
) -> Result<()> {
    let mut out = format!(
        "RTSP/1.0 {} {}\r\nCSeq: {cseq}\r\nServer: AirTunes/105.1\r\n",
        response.status, response.reason
    );
    for (name, value) in &response.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !response.body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&response.body);
    writer.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dmap_track_metadata() {
        let item = |tag: &[u8], value: &str| {
            let mut v = tag.to_vec();
            v.extend((value.len() as u32).to_be_bytes());
            v.extend(value.as_bytes());
            v
        };
        let mut inner = item(b"minm", "Song");
        inner.extend(item(b"asar", "Artist"));
        inner.extend(item(b"asal", "Album"));
        inner.extend(item(b"astm", "xxxx"));
        let mut body = b"mlit".to_vec();
        body.extend((inner.len() as u32).to_be_bytes());
        body.extend(inner);
        let np = parse_dmap(&body);
        assert_eq!(
            (
                np.title.as_deref(),
                np.artist.as_deref(),
                np.album.as_deref()
            ),
            (Some("Song"), Some("Artist"), Some("Album"))
        );
    }

    #[test]
    fn sender_addr_keeps_link_local_scope() {
        let peer: SocketAddr = "[fe80::14b5:d446:c598:5520%7]:49152".parse().unwrap();
        let SocketAddr::V6(addr) = sender_addr(peer, 6001) else {
            panic!()
        };
        assert_eq!((addr.port(), addr.scope_id()), (6001, 7));
        let mapped: SocketAddr = "[::ffff:192.168.5.53]:49152".parse().unwrap();
        assert_eq!(
            sender_addr(mapped, 6002),
            "192.168.5.53:6002".parse().unwrap()
        );
    }

    #[test]
    fn parses_transport_ports() {
        let t =
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002";
        assert_eq!(transport_field(t, "control_port"), Some("6001"));
        assert_eq!(transport_field(t, "timing_port"), Some("6002"));
        assert_eq!(transport_field(t, "server_port"), None);
    }
}
