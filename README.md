# unsonido

An audio bus for one room. AirPlay speakers, a radio/Icecast background and spoken alerts are
mixed into one output, using priority rules.

| Layer | Sources | Behaviour |
|---|---|---|
| background | Icecast/radio stream or looped file | plays when nothing above is active |
| foreground | AirPlay receivers (several at once, mixed) | crossfades the background out |
| notification | calendar reminders, chimes | ducks the layers below (-15 dB), then fades back |
| critical | alarms, outages | mutes everything else, raises master volume, preempts notifications |

## Run

```sh
cp unsonido.example.toml unsonido.toml
cargo run --release -- --config unsonido.toml
cargo run --release -- devices        # list output devices
```

Open <http://localhost:7700/> for the control page: volume, AirPlay status, background stream, alerts.

## Control page

The page in `ui/` is a React app (Vite + Tailwind CSS), organized in tabs: **Output** (volume +
AirPlay speakers), **Background** (radio/stream), **Alerts** (send, namespaces, queue) and two
reference tabs, **MCP Docs** (rendered live from `tools/list`) and **API Docs** (with copyable
curl examples). Its built output is committed in `ui/dist` and embedded into the binary, so
`cargo build` has no Node toolchain requirements.

```sh
pnpm --dir ui build     # rebuild the embedded page; then build/run the server again
pnpm --dir ui dev       # live-reloading dev server; proxies API calls to localhost:7700
```

For `pnpm dev` to be useful, run the server on the default port in another terminal. The page
keeps the same API contract as before: status polling, bearer token in the `unsonido-token`
localStorage key.

The example config defines two AirPlay speakers, **Unsonido Sala** and **Unsonido Cocina**. Pick
one in the AirPlay menu of an iPhone, iPad or Mac (Control Center → AirPlay, or the AirPlay
button in Music). Two devices can stream at once, one per speaker, and both are mixed. Add more
`[[airplay.receivers]]` for more simultaneous senders.

macOS asks once to allow incoming connections; allow it, or senders can't connect. Logging is
controlled with `RUST_LOG` (default `unsonido=info`; `unsonido=debug` shows the AirPlay protocol).

### Linux

Bonjour advertising uses Avahi's DNS-SD compatibility library:
`sudo apt install libavahi-compat-libdnssd-dev libasound2-dev` (Debian/Ubuntu). With
`[output] type = "null"` plus a pipe output, a headless server can publish the mix to Icecast.

## AirPlay

AirPlay 1 (RAOP), the same protocol shairport-sync's classic mode speaks. iOS, iPadOS and macOS
still stream to AirPlay 1 speakers.

- Encrypted (RSA + AES) and unencrypted sessions; ALAC and PCM audio.
- The sender's volume slider controls that speaker's level (`airplay.volume_range_db`).
- Lost packets are re-requested from the sender, and concealed with silence if they don't arrive.
- Audio plays on the sender's schedule (sync packets + clock exchange), so video apps like YouTube
  stay in lip sync. If your output device adds noticeable delay, trim it with `airplay.audio_delay_ms`.
- Clock drift between sender and output is corrected by dropping or repeating single frames.
- A new sender on a busy speaker takes it over.
- The track title, artist and album the sender transmits show in `/status` and on the control page.
- During a critical alert, playing senders are paused through their remote-control service (DACP)
  and resumed when the alert ends (`airplay.pause_on_critical`, on by default). The Music app on
  macOS and iOS apps that publish remote controls support this; others just get ducked.

AirPlay 2-only features (multi-room sync, HomeKit pairing) are not supported.

## Background (radio / Icecast)

```sh
curl -X PUT localhost:7700/background -H 'content-type: application/json' \
  -d '{"url":"https://ice1.somafm.com/groovesalad-128-mp3","gain_db":-6}'
# Fallbacks: tried in order when the ones before them fail; after an outage it returns to the first.
curl -X PUT localhost:7700/background -H 'content-type: application/json' \
  -d '{"urls":["https://ice5.somafm.com/groovesalad-128-mp3","https://ice1.somafm.com/groovesalad-128-mp3"]}'
curl -X PUT localhost:7700/background -H 'content-type: application/json' -d '{"file":"/path/song.mp3"}'
curl -X DELETE localhost:7700/background
```

MP3, AAC, Ogg Vorbis, FLAC and WAV streams are supported. The "now playing" title from Icecast
metadata shows in `/status`. Dropped streams reconnect with backoff.

## Pipe outputs (Icecast out, recording)

`[[pipe_outputs]]` pipes a copy of the final mix as raw f32le stereo into any program. The example
config publishes to an Icecast server with ffmpeg; the same works for recording to a file. If the
program exits, it is restarted.

## Alert API

```sh
# Spoken notification (chime + TTS). `id` deduplicates repeats for `alerts.dedupe_seconds`.
curl -X POST localhost:7700/alerts -H 'content-type: application/json' \
  -d '{"id":"cal:standup:2min","text":"Standup in two minutes"}'

# Critical, repeated, with a specific provider and voice.
curl -X POST localhost:7700/alerts -H 'content-type: application/json' \
  -d '{"priority":"critical","text":"Production is down","provider":"eleven","repeat":2}'

# Play an audio file from a URL, or just a chime.
curl -X POST localhost:7700/alerts -H 'content-type: application/json' -d '{"url":"https://example.com/doorbell.mp3"}'
curl -X POST localhost:7700/alerts -H 'content-type: application/json' -d '{}'

# Upload audio you generated yourself (wav, mp3, aiff, flac, ogg, m4a).
curl -X POST 'localhost:7700/alerts/audio?priority=critical&chime=true' \
  -H 'content-type: audio/mpeg' --data-binary @alert.mp3

curl -X DELETE localhost:7700/alerts/current   # stop the alert playing now
curl -X DELETE localhost:7700/alerts           # stop it and clear the queue
curl -X PUT localhost:7700/volume -H 'content-type: application/json' -d '{"volume":0.6}'
curl localhost:7700/status
curl localhost:7700/tts/providers
```

When `server.token` or `server.token_env` is set, send `Authorization: Bearer <token>`. The
control page asks for the token once and keeps it in the browser.

Responses: `202 {"status":"queued","id",...}` when queued, `200 {"status":"suppressed","reason":"duplicate"|"quiet_hours"}`
when policy drops the alert, `400` for invalid input, `429` when the queue is full and `502` when TTS or a download fails.

## Alert namespaces (kubectl-style)

Alerts can be applied into a namespace under a name — `calendar`, `agents`, `script`, … — like kubectl
resources. Applying queues the alert for playback and stores it; applying the same name again replaces
the stored alert (an identical apply is a no-op); deleting cancels it (or stops it if it is playing);
and once an alert has played it self-destructs out of its namespace. A namespace exists while it holds
alerts.

```sh
# Apply: play it, and keep it as calendar/standup until it does.
curl -X PUT localhost:7700/namespaces/calendar/alerts/standup \
  -H 'content-type: application/json' -d '{"text":"Standup in two minutes"}'

# POST works too; re-applying the same name replaces the stored alert.
curl -X POST localhost:7700/namespaces/agents/alerts/task-done \
  -H 'content-type: application/json' -d '{"text":"Task done","priority":"critical"}'

# List: everything, one namespace, one alert.
curl localhost:7700/namespaces
curl localhost:7700/namespaces/calendar
curl localhost:7700/namespaces/calendar/alerts/standup

# Delete: a waiting alert leaves the queue, a playing one is stopped.
curl -X DELETE localhost:7700/namespaces/calendar/alerts/standup
curl -X DELETE localhost:7700/namespaces/calendar   # the namespace and everything in it
```

### Scheduled alerts (`at`)

Apply with `at` to schedule the alert for a future moment instead of playing it now — upload all of
today's calendar events at once, and each one announces itself when its time comes, then disappears:

```sh
curl -X PUT localhost:7700/namespaces/calendar/alerts/dentist -H 'content-type: application/json' \
  -d '{"text":"Dentist at four","at":"2026-11-02 15:45"}'
```

`at` accepts RFC 3339 (`2026-11-02T15:45:00-06:00`, `…Z`) or local time (`2026-11-02 15:45`,
`2026-11-02T15:45:00`, or a plain date for midnight); a time in the past plays immediately, like an
event that is already late. Scheduled entries wait as `"state": "scheduled"` with their `due_at`, and
the dispatcher checks every second. Quiet hours are applied when the alert fires, not when it is
applied; a notification that falls inside them is dropped with a log line. If rendering fails at the
due moment (TTS down, unreachable url) the alert retries every 30 s up to three times, then
self-destructs with an error in the log.

### Syncing a whole namespace

`PUT /namespaces/{ns}` replaces the namespace wholesale, the way `kubectl apply -f dir/` prunes:
every alert in the set is applied (created or replaced; identical ones left untouched), and stored
alerts that are not in the set are deleted. An empty set empties the namespace.

```sh
curl -X PUT localhost:7700/namespaces/calendar -H 'content-type: application/json' -d '{
  "alerts": {
    "standup": {"text": "Standup at nine",      "at": "2026-11-02 08:55"},
    "lunch":   {"text": "Lunch with Ana",      "at": "2026-11-02 13:00"},
    "dentist": {"text": "Dentist moved to 5",  "at": "2026-11-02 16:45", "priority": "critical"}
  }
}'
# -> {"namespace":"calendar","results":{"dentist":{"status":"scheduled",...},
#      "lunch":{"status":"scheduled",...},"standup":{"status":"scheduled",...}},"pruned":["old-event"]}
```

Named alerts take the same body as `POST /alerts` (`text`, `url`, `priority`, `provider`, `voice`,
`repeat`, `chime`, and now `at`, which is only valid on a named alert) and the same policies. They
skip `id`-deduplication — re-applying a name is an explicit replace. The engine id is
`<namespace>/<name>#<n>`, so a replaced alert can be cancelled while its replacement is already
queued. Uploaded audio can be named and scheduled too: add
`?namespace=calendar&name=doorbell&at=2026-11-02+18:00` to `POST /alerts/audio`.

The control page applies alerts the same way: fill the namespace and name fields under "Send alert",
optionally pick a time, and manage what waits under "Namespaces" — playing, pending and scheduled.

## MCP (Model Context Protocol)

Agents can drive the whole alert API as discovered tools: a stateless MCP server speaks the
"Streamable HTTP" transport on `http://127.0.0.1:7700/mcp` (JSON-RPC 2.0, plain JSON responses).
Register it once with an agent such as Claude Code:

```sh
claude mcp add --transport http unsonido http://127.0.0.1:7700/mcp
# with the API token enabled, send it as a header:
claude mcp add --transport http unsonido http://127.0.0.1:7700/mcp \
  --header "Authorization: Bearer <token>"
```

or drop it into a project's `.mcp.json`:

```json
{ "mcpServers": { "unsonido": { "type": "http", "url": "http://127.0.0.1:7700/mcp" } } }
```

The agent then gets thirteen tools mirroring the HTTP API: `play_alert` (speak now), `apply_alert`
(store / schedule with `at`, kubectl-style), `sync_namespace` (replace a namespace wholesale),
`get_alert`, `delete_alert`, `list_namespaces`, `delete_namespace`, `get_status`, `skip_current`,
`clear_alerts`, `set_volume`, `play_background` and `stop_background`. Tool failures come back as
`isError` results with the same messages the HTTP API produces. Like the rest of the control API,
the endpoint is reachable from localhost only (and sits behind the bearer token when one is
configured).

## Text-to-speech

Providers are named instances in `[tts.providers.<name>]`, so you can define several voices of the same backend.

- `say`: macOS built-in.
- `command`: any local program (Piper, espeak-ng, custom scripts). Placeholders: `{output}`, `{text}`, `{voice}`.
- `elevenlabs`: ElevenLabs API.
- `openai`: any OpenAI-compatible `/audio/speech` endpoint, including local servers.

If a provider fails, the `fallback` chain is tried in order. With `cache_dir` set, synthesized
speech is cached by provider config, voice and text, so repeated alerts cost nothing.

## Architecture

```
AirPlay receivers ─┐  (UDP/RTSP → decrypt → ALAC → resample)
Radio / file ──────┼─▶ StreamWriter ─▶ lock-free ring ─▶ StreamSource ─┐
                   │                                                   ├─▶ Mixer ─▶ device / null / wav
HTTP API ─▶ AlertService (policy, TTS, decode) ─▶ bounded channel ─────┘     └─▶ pipe outputs (ffmpeg…)
```

- `engine.rs`: real-time mixer with named inputs and priority layers. It never blocks or allocates per block.
- `stream.rs`: ring buffer, prebuffering, underrun handling, drift correction and streaming resampling.
- `airplay/`: Bonjour advertising, RTSP, crypto, RTP/retransmission, decoding.
- `inputs.rs`: radio/file background with ICY metadata and reconnects.
- `taps.rs`: pipe outputs.
- `alerts.rs`, `tts/`: alert policy and speech providers.
- `ui.html`: the control page.

`cargo test` includes an end-to-end AirPlay session: a simulated iPhone checks the
Apple-Challenge signature, sends an RSA-wrapped key, streams encrypted ALAC, checks that a
dropped packet is re-requested and the audio arrives bit-exact, and that a critical alert pauses
and resumes it over DACP via a real Bonjour lookup.

## Roadmap

1. ~~Mixer core, priority layers, ducking, local output~~
2. ~~Alert API and pluggable TTS~~
3. ~~Streaming inputs: Icecast/radio with reconnects~~
4. ~~AirPlay 1 receivers, several at once~~
5. ~~Pipe outputs (Icecast publishing, recording)~~
6. ~~Web control page~~
7. ~~Pause/resume AirPlay senders during critical alerts (DACP), track metadata from AirPlay~~
8. Possible later: AirPlay 2 (multi-room sync, HomeKit pairing), artwork
