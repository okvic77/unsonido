//! End-to-end test: a fake iPhone runs a full AirPlay 1 session against a receiver: challenge,
//! RSA-wrapped AES key, encrypted ALAC over RTP, and a dropped packet that must be re-requested.

use std::net::UdpSocket;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Oaep};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::crypto::tests::{encrypt_packet, public_key};
use super::{mac_for, start};
use crate::config::AirplayConfig;
use crate::engine::Command;

const FRAMES: usize = 352;
const PACKETS: u16 = 40;
const DROPPED: u16 = 10;

struct Rtsp {
    stream: TcpStream,
    cseq: u32,
}

impl Rtsp {
    async fn request(
        &mut self,
        method: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> (u16, String) {
        self.cseq += 1;
        let mut req = format!(
            "{method} rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: TestPhone\r\n",
            self.cseq
        );
        for (name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);
        self.stream.write_all(req.as_bytes()).await.unwrap();

        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            self.stream.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
        }
        let head = String::from_utf8(buf).unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head)
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
}

/// Uncompressed ALAC frame: CPE element, no shift, raw interleaved 16-bit samples, END tag.
fn alac_frame(samples: &[i16]) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.put(1, 3); // CPE (stereo pair)
    bits.put(0, 4); // element instance
    bits.put(0, 12); // unused
    bits.put(0, 1); // not a partial frame
    bits.put(0, 2); // no shift
    bits.put(1, 1); // uncompressed
    for &s in samples {
        bits.put(s as u16 as u32, 16);
    }
    bits.put(7, 3); // END
    bits.bytes
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.used % 8);
            self.used += 1;
        }
    }
}

fn tone(packet: u16) -> Vec<i16> {
    (0..FRAMES)
        .flat_map(|i| {
            let n = packet as usize * FRAMES + i;
            let s = ((n as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 8000.0) as i16;
            [s, -s]
        })
        .collect()
}

fn rtp_packet(seq: u16, key: [u8; 16], iv: [u8; 16]) -> Vec<u8> {
    let mut payload = alac_frame(&tone(seq));
    encrypt_packet(key, iv, &mut payload);
    let mut packet = vec![0x80, if seq == 0 { 0xe0 } else { 0x60 }];
    packet.extend(seq.to_be_bytes());
    packet.extend((seq as u32 * FRAMES as u32).to_be_bytes());
    packet.extend(0x1234_5678u32.to_be_bytes());
    packet.extend(payload);
    packet
}

const DACP_ID: &str = "A1B2C3D4E5F60718";

/// A sender's DACP remote-control endpoint, advertised over Bonjour like the Music app does.
struct FakeRemote {
    requests: tokio::sync::mpsc::Receiver<String>,
    _registration: super::mdns::Registration,
}

impl FakeRemote {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let registration =
            super::mdns::register(&format!("iTunes_Ctrl_{DACP_ID}"), "_dacp._tcp", port, &[])
                .unwrap();
        let (tx, requests) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).await.is_ok() {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let line = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(" HTTP/1.1")
                    .to_string();
                let remote = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("active-remote: ")
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let _ = stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = tx.send(format!("{line} active-remote={remote}")).await;
            }
        });
        Self {
            requests,
            _registration: registration,
        }
    }

    async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("DACP request within 5s")
            .unwrap()
    }
}

async fn eventually(check: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_alac_session_with_retransmission() {
    let port = free_port();
    let cfg = AirplayConfig {
        advertise: false,
        base_port: port,
        buffer_ms: 50,
        volume_range_db: 60.0,
        pause_on_critical: false,
        audio_delay_ms: 0,
        name: "Test".into(),
        max_receivers: 1,
        idle_grace_secs: 0,
    };
    let (tx, rx) = crossbeam_channel::bounded(8);
    let airplay = start(&cfg, 44_100, &tx).await.unwrap();
    let Ok(Command::AddSource { mut source, .. }) = rx.try_recv() else {
        panic!("no source registered")
    };

    let mut rtsp = Rtsp {
        stream: TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
        cseq: 0,
    };

    // 1. Challenge: the response must be the AirPort key's signature over challenge+ip+mac.
    let challenge = [0x5au8; 16];
    let (status, head) = rtsp
        .request(
            "OPTIONS",
            &[("Apple-Challenge", STANDARD_NO_PAD.encode(challenge))],
            "",
        )
        .await;
    assert_eq!(status, 200);
    let signature =
        super::crypto::b64_decode(header(&head, "Apple-Response").expect("Apple-Response"))
            .unwrap();
    let pk = public_key();
    let signed = BigUint::from_bytes_be(&signature)
        .modpow(pk.e(), pk.n())
        .to_bytes_be();
    let mut expected = challenge.to_vec();
    expected.extend([127, 0, 0, 1]);
    expected.extend(mac_for("Test"));
    expected.resize(32, 0);
    assert_eq!(&signed[signed.len() - 32..], &expected[..]);

    // 2. ANNOUNCE with an RSA-wrapped AES key.
    let (key, iv) = ([0x11u8; 16], [0x22u8; 16]);
    let wrapped = pk
        .encrypt(&mut rsa::rand_core::OsRng, Oaep::new::<sha1::Sha1>(), &key)
        .unwrap();
    let sdp = format!(
        "v=0\r\no=iTunes 1 0 IN IP4 127.0.0.1\r\ns=iTunes\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
         a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
        STANDARD_NO_PAD.encode(wrapped),
        STANDARD_NO_PAD.encode(iv)
    );
    let (status, _) = rtsp
        .request(
            "ANNOUNCE",
            &[("Content-Type", "application/sdp".into())],
            &sdp,
        )
        .await;
    assert_eq!(status, 200);

    // 3. SETUP with our control/timing ports.
    let control = UdpSocket::bind("127.0.0.1:0").unwrap();
    let timing = UdpSocket::bind("127.0.0.1:0").unwrap();
    let transport = format!(
        "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
        control.local_addr().unwrap().port(),
        timing.local_addr().unwrap().port()
    );
    let (status, head) = rtsp.request("SETUP", &[("Transport", transport)], "").await;
    assert_eq!(status, 200);
    let server_transport = header(&head, "Transport").unwrap();
    let server_port = |name: &str| -> u16 {
        super::rtsp::transport_field(server_transport, name)
            .unwrap()
            .parse()
            .unwrap()
    };
    let (audio_port, control_port) = (server_port("server_port"), server_port("control_port"));

    let (status, _) = rtsp
        .request(
            "RECORD",
            &[
                ("Range", "npt=0-".into()),
                ("DACP-ID", DACP_ID.into()),
                ("Active-Remote", "4242".into()),
            ],
            "",
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = rtsp
        .request(
            "SET_PARAMETER",
            &[("Content-Type", "text/parameters".into())],
            "volume: -15.000000\r\n",
        )
        .await;
    assert_eq!(status, 200);

    // 4. Stream, holding back one packet until the receiver asks for it.
    let audio = UdpSocket::bind("127.0.0.1:0").unwrap();
    for seq in 0..PACKETS {
        if seq != DROPPED {
            audio
                .send_to(&rtp_packet(seq, key, iv), ("127.0.0.1", audio_port))
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    control
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = [0u8; 64];
    let (n, _) = control
        .recv_from(&mut request)
        .expect("receiver asks for the missing packet");
    assert_eq!((n, request[1]), (8, 0xd5));
    let first = u16::from_be_bytes([request[4], request[5]]);
    let count = u16::from_be_bytes([request[6], request[7]]);
    assert_eq!((first, count), (DROPPED, 1));
    let mut resend = vec![0x80, 0xd6, 0, 1];
    resend.extend(rtp_packet(DROPPED, key, iv));
    control
        .send_to(&resend, ("127.0.0.1", control_port))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 5. The mixer side gets the exact tone back, scaled by the volume, with no gap. The queue
    // starts far above the target latency, so drift correction may skip single frames.
    let mut out = vec![0.0f32; PACKETS as usize * FRAMES * 2];
    assert!(source.render(&mut out), "source reports active");
    let gain = crate::audio::db_to_gain(-30.0);
    let expected: Vec<[f32; 2]> = (0..PACKETS)
        .flat_map(tone)
        .map(|s| s as f32 / 32768.0 * gain)
        .collect::<Vec<_>>()
        .as_chunks::<2>()
        .0
        .to_vec();
    let close =
        |a: &[f32; 2], b: &[f32; 2]| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4;
    let (mut j, mut skipped) = (0usize, 0usize);
    // The last few frames may be silence: skipped frames leave the queue short at the end.
    for frame in out.as_chunks::<2>().0.iter().take(expected.len() - 100) {
        if j >= expected.len() {
            break;
        }
        if !close(frame, &expected[j]) {
            assert!(
                j + 1 < expected.len() && close(frame, &expected[j + 1]),
                "mismatch at expected frame {j}"
            );
            j += 1;
            skipped += 1;
        }
        j += 1;
    }
    assert!(
        j > (DROPPED as usize + 2) * FRAMES,
        "playback covered the retransmitted packet"
    );
    assert!(
        skipped <= out.len() / 2 / 2000 + 1,
        "skipped {skipped} frames"
    );

    let status = &airplay.status()[0];
    assert_eq!(status.volume_db, -15.0);
    assert!(status.client.as_deref().unwrap().contains("TestPhone"));
    assert!(status.remote_control);

    // 6. A critical alert pauses the sender over DACP, and its end resumes it.
    let mut remote = FakeRemote::start().await;
    let (critical_tx, critical_rx) = tokio::sync::watch::channel(false);
    airplay.pause_during_critical_alerts(critical_rx);
    critical_tx.send(true).unwrap();
    assert_eq!(
        remote.next().await,
        "GET /ctrl-int/1/pause active-remote=4242"
    );
    assert!(eventually(|| airplay.status()[0].paused_for_alert).await);
    critical_tx.send(false).unwrap();
    assert_eq!(
        remote.next().await,
        "GET /ctrl-int/1/play active-remote=4242"
    );

    // 7. Moving the speaker's slider on the control page moves the sender's volume too.
    airplay
        .receiver("Test")
        .unwrap()
        .update(&super::ReceiverUpdate {
            volume: Some(0.5),
            muted: None,
        })
        .await;
    assert_eq!(
        remote.next().await,
        "GET /ctrl-int/1/setproperty?dmcp.device-volume=-15.000000 active-remote=4242"
    );
    let status = &airplay.status()[0];
    assert_eq!((status.volume, status.volume_synced), (0.5, Some(true)));
    assert!(eventually(|| !airplay.status()[0].paused_for_alert).await);

    let (status, _) = rtsp.request("TEARDOWN", &[], "").await;
    assert_eq!(status, 200);
    assert!(airplay.status()[0].client.is_none());
}

/// The pool keeps one idle speaker: it grows when that one gets a sender and shrinks back once
/// extra speakers are idle again.
#[tokio::test(flavor = "multi_thread")]
async fn pool_grows_when_busy_and_shrinks_when_idle() {
    let cfg = AirplayConfig {
        advertise: false,
        base_port: free_port_range(3),
        name: "Pool".into(),
        max_receivers: 3,
        idle_grace_secs: 0,
        ..AirplayConfig::default()
    };
    let (tx, _rx) = crossbeam_channel::bounded(64);
    let pool = start(&cfg, 44_100, &tx).await.unwrap();
    let names = |pool: &super::AirPlay| {
        pool.status()
            .into_iter()
            .map(|r| r.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&pool), ["Pool"]);

    let occupy = |name: &str, id: u64| {
        let receiver = pool.receiver(name).unwrap();
        receiver.begin_session(
            id,
            super::session::AudioSession::idle_for_tests(),
            super::SenderInfo::default(),
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        receiver
    };
    let first = occupy("Pool", 1);
    pool.rebalance().await.unwrap();
    assert_eq!(names(&pool), ["Pool", "Pool 2"]);
    let second = occupy("Pool 2", 2);
    pool.rebalance().await.unwrap();
    assert_eq!(names(&pool), ["Pool", "Pool 2", "Pool 3"]);
    let _third = occupy("Pool 3", 3);
    pool.rebalance().await.unwrap();
    assert_eq!(names(&pool).len(), 3, "never more than max_receivers");

    // Freeing two leaves two idle; the lower-numbered idle one stays.
    first.end_session(1);
    second.end_session(2);
    pool.rebalance().await.unwrap();
    assert_eq!(names(&pool), ["Pool", "Pool 3"]);

    // Volume and mute from the control page show in status.
    let speaker = pool.receiver("Pool").unwrap();
    speaker
        .update(&super::ReceiverUpdate {
            volume: Some(0.5),
            muted: Some(true),
        })
        .await;
    let status = speaker.status();
    assert_eq!(
        (status.volume, status.volume_db, status.muted),
        (0.5, -15.0, true)
    );
}

/// A free block of consecutive TCP ports for a pool, below the ephemeral range so ports handed
/// out to other tests running in parallel cannot collide with it.
fn free_port_range(count: u16) -> u16 {
    let start = 20_000 + (std::process::id() % 5_000) as u16 * 4;
    (start..40_000)
        .step_by(count as usize)
        .find(|&base| {
            (0..count).all(|i| std::net::TcpListener::bind(("0.0.0.0", base + i)).is_ok())
        })
        .expect("a free port range")
}
