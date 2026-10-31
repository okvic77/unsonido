//! DACP: the remote-control service AirPlay senders (Music, iTunes, iOS) publish so a speaker
//! can pause, resume or change volume. The sender sends `DACP-ID` and `Active-Remote` headers
//! over RTSP and advertises `iTunes_Ctrl_<DACP-ID>._dacp._tcp`; commands are plain HTTP GETs.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use super::SenderInfo;
use super::mdns;

/// Send a DACP command such as `pause`, `play` or `playpause`.
pub async fn send(sender: &SenderInfo, command: &str) -> Result<()> {
    Endpoint::resolve(sender)
        .await?
        .get(command)
        .await
        .map(drop)
}

/// Move the sender's volume slider for this speaker to `volume_db` (-30..0, -144 = mute).
/// Returns which method the sender accepted.
pub async fn set_volume(
    sender: &SenderInfo,
    speaker: &str,
    volume_db: f32,
) -> Result<&'static str> {
    let endpoint = Endpoint::resolve(sender).await?;
    // Speakers with their own volume control report it as the device volume.
    let device = endpoint
        .get(&format!("setproperty?dmcp.device-volume={volume_db:.6}"))
        .await;
    if device.is_ok() {
        return Ok("device-volume");
    }
    // Music/iTunes style: find this speaker in the sender's list and set its volume (0..100).
    let speakers = endpoint
        .get("getspeakers")
        .await
        .context("listing the sender's speakers")?;
    let id = speaker_id(&speakers, speaker)
        .ok_or_else(|| anyhow!("{speaker:?} is not in the sender's speaker list"))?;
    let percent = if volume_db <= -30.0 {
        0
    } else {
        (((volume_db + 30.0) / 30.0) * 100.0).round() as u32
    };
    endpoint
        .get(&format!(
            "setproperty?speaker-id={id}&dmcp.volume={percent}"
        ))
        .await
        .context("setting the speaker volume")?;
    Ok("speaker-volume")
}

struct Endpoint {
    base: String,
    active_remote: String,
}

impl Endpoint {
    async fn resolve(sender: &SenderInfo) -> Result<Self> {
        let (Some(dacp_id), Some(active_remote), Some(ip)) =
            (&sender.dacp_id, &sender.active_remote, sender.ip)
        else {
            return Err(anyhow!("sender did not provide DACP details"));
        };
        let service = format!("iTunes_Ctrl_{dacp_id}");
        let port = tokio::task::spawn_blocking(move || {
            mdns::resolve_port(&service, "_dacp._tcp", Duration::from_secs(3))
        })
        .await
        .context("resolver task failed")??;
        let host = match ip {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        };
        Ok(Self {
            base: format!("http://{host}:{port}/ctrl-int/1/"),
            active_remote: active_remote.clone(),
        })
    }

    async fn get(&self, command: &str) -> Result<Vec<u8>> {
        let url = format!("{}{command}", self.base);
        let response = reqwest::Client::new()
            .get(&url)
            .header("Active-Remote", &self.active_remote)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("{url} returned {}", response.status()));
        }
        Ok(response.bytes().await?.to_vec())
    }
}

/// Find a speaker's id by name in a DMAP `getspeakers` reply: `casp` holds one `mdcl` per
/// speaker, each with a name (`minm`) and a 64-bit id (`msma`).
fn speaker_id(dmap: &[u8], name: &str) -> Option<u64> {
    fn items(mut data: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = Vec::new();
        while data.len() >= 8 {
            let len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
            let Some(value) = data.get(8..8 + len) else {
                break;
            };
            out.push((&data[..4], value));
            data = &data[8 + len..];
        }
        out
    }
    let (_, list) = items(dmap).into_iter().find(|(tag, _)| *tag == b"casp")?;
    items(list)
        .into_iter()
        .filter(|(tag, _)| *tag == b"mdcl")
        .find_map(|(_, speaker)| {
            let fields = items(speaker);
            let field = |tag: &[u8]| fields.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v);
            if field(b"minm")? != name.as_bytes() {
                return None;
            }
            let id = field(b"msma")?;
            let mut bytes = [0u8; 8];
            bytes[8 - id.len().min(8)..].copy_from_slice(&id[id.len().saturating_sub(8)..]);
            Some(u64::from_be_bytes(bytes))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(tag: &[u8], value: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend((value.len() as u32).to_be_bytes());
        v.extend(value);
        v
    }

    #[test]
    fn finds_speaker_id_by_name() {
        let mut list = Vec::new();
        for (name, id) in [("Computer", 0u64), ("Unsonido", 0x1234_5678_9abc)] {
            let mut speaker = item(b"minm", name.as_bytes());
            speaker.extend(item(b"msma", &id.to_be_bytes()));
            speaker.extend(item(b"cmvo", &50u32.to_be_bytes()));
            list.extend(item(b"mdcl", &speaker));
        }
        let mut body = item(b"mstt", &200u32.to_be_bytes());
        body.extend(list);
        let reply = item(b"casp", &body);
        assert_eq!(speaker_id(&reply, "Unsonido"), Some(0x1234_5678_9abc));
        assert_eq!(speaker_id(&reply, "Missing"), None);
    }
}
