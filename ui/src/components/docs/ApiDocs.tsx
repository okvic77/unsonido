import { Fragment } from "react";
import { CodeBlock, Section, Tag } from "../widgets";

/** Human reference for the HTTP control API (see src/api.rs). */

interface Endpoint {
  method: "GET" | "POST" | "PUT" | "DELETE";
  path: string;
  summary: string;
  detail?: string;
}

interface Group {
  title: string;
  endpoints: Endpoint[];
  examples: string[];
}

const GROUPS: Group[] = [
  {
    title: "Alerts",
    endpoints: [
      {
        method: "POST",
        path: "/alerts",
        summary: "Queue a spoken alert: chime + TTS of `text`, or play an audio `url`.",
        detail:
          "JSON body: id (dedupe), text, url, provider, voice, priority (notification | critical), repeat, chime (default true).",
      },
      {
        method: "POST",
        path: "/alerts/audio",
        summary: "Play your own audio: raw body (wav, mp3, aiff, flac, ogg, m4a).",
        detail:
          "Query params: id, namespace + name, at (needs a name), priority, repeat, chime, format (from Content-Type when omitted).",
      },
      {
        method: "DELETE",
        path: "/alerts/current",
        summary: "Skip the alert that is playing now.",
      },
      {
        method: "DELETE",
        path: "/alerts",
        summary: "Skip it and clear the queue.",
      },
    ],
    examples: [
      "curl -X POST localhost:7700/alerts -H 'content-type: application/json' \\\n  -d '{\"id\":\"cal:standup:2min\",\"text\":\"Standup in two minutes\"}'",
      "curl -X POST localhost:7700/alerts -H 'content-type: application/json' \\\n  -d '{\"priority\":\"critical\",\"text\":\"Production is down\",\"provider\":\"eleven\",\"repeat\":2}'",
      "curl -X POST localhost:7700/alerts -H 'content-type: application/json' \\\n  -d '{\"url\":\"https://example.com/doorbell.mp3\"}'",
      "curl -X POST 'localhost:7700/alerts/audio?priority=critical&chime=true' \\\n  -H 'content-type: audio/mpeg' --data-binary @alert.mp3",
      "curl -X DELETE localhost:7700/alerts/current   # stop the alert playing now",
      "curl -X DELETE localhost:7700/alerts           # stop it and clear the queue",
    ],
  },
  {
    title: "Namespaces (kubectl-style)",
    endpoints: [
      {
        method: "PUT",
        path: "/namespaces/{ns}/alerts/{name}",
        summary: "Apply: play now (or at `at`), and store under that name.",
        detail:
          "Same alert body as POST /alerts plus at (RFC 3339 or \"YYYY-MM-DD HH:MM\"; past plays now). Re-applying replaces; identical applies are a no-op; played alerts self-destruct. POST works too.",
      },
      {
        method: "GET",
        path: "/namespaces/{ns}/alerts/{name}",
        summary: "One stored alert, or 404.",
      },
      {
        method: "DELETE",
        path: "/namespaces/{ns}/alerts/{name}",
        summary: "Delete: a waiting one leaves, a playing one stops, a scheduled one never happens.",
      },
      {
        method: "GET",
        path: "/namespaces",
        summary: "Every namespace with its stored alerts.",
      },
      {
        method: "PUT",
        path: "/namespaces/{ns}",
        summary: "Sync: apply a whole set and prune the rest, e.g. today's calendar in one call.",
        detail: 'Body: {"alerts": { "<name>": <alert body> }} — returns a per-alert report.',
      },
      {
        method: "DELETE",
        path: "/namespaces/{ns}",
        summary: "Delete the namespace and everything stored in it.",
      },
    ],
    examples: [
      "curl -X PUT localhost:7700/namespaces/calendar/alerts/standup \\\n  -H 'content-type: application/json' -d '{\"text\":\"Standup in two minutes\"}'",
      "curl -X PUT localhost:7700/namespaces/calendar/alerts/standup \\\n  -H 'content-type: application/json' -d '{\"text\":\"Standup!\",\"at\":\"2026-11-02 08:55\"}'",
      "curl localhost:7700/namespaces",
      "curl -X DELETE localhost:7700/namespaces/calendar/alerts/standup",
    ],
  },
  {
    title: "Playback",
    endpoints: [
      {
        method: "GET",
        path: "/status",
        summary: "Everything: queue, namespaces, history, volume, background and AirPlay.",
      },
      {
        method: "PUT",
        path: "/volume",
        summary: "Master volume, 0.0 to 1.0.",
        detail: 'Body: {"volume": 0.6}',
      },
      {
        method: "PUT",
        path: "/background",
        summary: "Background radio/Icecast stream, or a looped local file.",
        detail:
          'Body: {"url": "…"} or {"urls": ["…", "…"]} (fallbacks in order) or {"file": "/path"}, optional gain_db (default -6).',
      },
      {
        method: "DELETE",
        path: "/background",
        summary: "Stop the background stream.",
      },
      {
        method: "PUT",
        path: "/airplay/{name}",
        summary: "Per-speaker trim: volume and/or muted (the sender's own slider stays in sync).",
        detail: 'Body: {"volume": 0.5, "muted": false} — omitted fields stay as they are.',
      },
      {
        method: "DELETE",
        path: "/airplay/{name}/session",
        summary: "Disconnect the sender from that speaker.",
      },
    ],
    examples: [
      "curl -X PUT localhost:7700/volume -H 'content-type: application/json' -d '{\"volume\":0.6}'",
      'curl -X PUT localhost:7700/background -H \'content-type: application/json\' \\\n  -d \'{"urls":["https://ice5.somafm.com/groovesalad-128-mp3","https://ice1.somafm.com/groovesalad-128-mp3"]}\'',
      "curl -X DELETE localhost:7700/background",
    ],
  },
  {
    title: "Reference",
    endpoints: [
      {
        method: "GET",
        path: "/tts/providers",
        summary: "Configured TTS providers (name, kind, which is default).",
      },
      {
        method: "POST",
        path: "/mcp",
        summary: "Model Context Protocol: JSON-RPC 2.0 with tools for agents. See the MCP Docs tab.",
      },
    ],
    examples: ["curl localhost:7700/tts/providers"],
  },
];

const BADGE: Record<Endpoint["method"], "ok" | "accent" | "muted" | "critical"> = {
  GET: "ok",
  POST: "accent",
  PUT: "muted",
  DELETE: "critical",
};

export function ApiDocs() {
  return (
    <>
      <Section title="Authentication">
        <p className="text-sm text-muted">
          When <code className="font-mono not-italic text-ink">server.token</code> or{" "}
          <code className="font-mono not-italic text-ink">server.token_env</code> is set, send{" "}
          <code className="font-mono not-italic text-ink">Authorization: Bearer &lt;token&gt;</code>{" "}
          with every call — the control page supplies it automatically.
        </p>
      </Section>

      {GROUPS.map((group) => (
        <Section key={group.title} title={group.title}>
          <ul className="divide-y divide-line/70">
            {group.endpoints.map((e) => (
              <Fragment key={e.method + e.path}>
                <li className="flex flex-wrap items-baseline gap-2 py-2.5">
                  <Tag kind={BADGE[e.method]}>{e.method}</Tag>
                  <code className="min-w-0 grow font-mono text-[13px] break-all text-ink">
                    {e.path}
                  </code>
                </li>
                <li className="pb-2.5 pl-0.5 text-sm text-muted">
                  <span>{e.summary}</span>
                  {e.detail && <p className="mt-1 text-xs opacity-80">{e.detail}</p>}
                </li>
              </Fragment>
            ))}
          </ul>
          <div className="mt-2 flex flex-col gap-2">
            {group.examples.map((code, i) => (
              <CodeBlock key={i} code={code} />
            ))}
          </div>
        </Section>
      ))}

      <Section title="Responses">
        <p className="text-sm text-muted">
          <code className="font-mono not-italic text-ink">202</code> when an alert was queued,{" "}
          <code className="font-mono not-italic text-ink">200</code> when it was dropped by policy
          (a <code className="font-mono not-italic text-ink">{"{\"status\":\"suppressed\",\"reason\":…}"}</code>{" "}
          outcome), <code className="font-mono not-italic text-ink">400</code> for invalid input,{" "}
          <code className="font-mono not-italic text-ink">429</code> when the queue is full and{" "}
          <code className="font-mono not-italic text-ink">502</code> when TTS or a download fails.
          Errors arrive as <code className="font-mono not-italic text-ink">{"{\"error\": \"…\"}"}</code>.
        </p>
      </Section>
    </>
  );
}