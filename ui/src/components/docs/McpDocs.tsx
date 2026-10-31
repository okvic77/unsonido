import { useEffect, useState } from "react";
import { api } from "../../api";
import type { McpTool } from "../../types";
import { CodeBlock, Section, Tag } from "../widgets";

/** Reference for the MCP endpoint (see src/mcp.rs). */

const TRANSPORT_CODE = `POST /mcp   # JSON-RPC 2.0 (stateless Streamable HTTP)
# answers application/json; text/event-stream if the client refuses JSON
# implements initialize, ping, tools/list and tools/call`;

const CLIENT_CODE = `{
  "mcpServers": {
    "unsonido": { "url": "http://localhost:7700/mcp" }
  }
}`;

const EXAMPLE_CODE = `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":
  { "name":"play_alert",
    "arguments":{"text":"Standup in two minutes","priority":"notification"}}}`;

/** Fallback shown when the live tools/list cannot be fetched (e.g. older binary). */
const FALLBACK: McpTool[] = [
  { name: "play_alert", description: "Speak text or play an audio URL through the speakers right now.", inputSchema: { type: "object", properties: { text: "string — text to speak", url: "http(s) audio file", provider: "TTS provider name", voice: "string", priority: "notification | critical", repeat: "integer ≥ 1", chime: "boolean (default true)" } } },
  { name: "apply_alert", description: "Store an alert in a namespace under a name (kubectl-style): it plays now, or at `at`; re-applying replaces the stored alert, and once it plays it self-destructs.", inputSchema: { type: "object", properties: { namespace: "string, e.g. calendar, agents, script", name: "string, unique in the namespace", at: "RFC 3339 or \"YYYY-MM-DD HH:MM\"", text: "string", url: "http(s) audio file", priority: "notification | critical", repeat: "integer", chime: "boolean" }, required: ["namespace", "name"] } },
  { name: "sync_namespace", description: "Replace a namespace wholesale: apply every listed alert and prune the ones missing from the set — e.g. re-upload today's calendar in one call.", inputSchema: { type: "object", properties: { namespace: "string", alerts: "name → alert body, with optional at" }, required: ["namespace", "alerts"] } },
  { name: "get_alert", description: "Return one stored alert, or {\"found\": false}.", inputSchema: { type: "object", properties: { namespace: "string", name: "string" }, required: ["namespace", "name"] } },
  { name: "delete_alert", description: "Delete one stored alert: a waiting one leaves the queue, a playing one stops, a scheduled one never happens.", inputSchema: { type: "object", properties: { namespace: "string", name: "string" }, required: ["namespace", "name"] } },
  { name: "list_namespaces", description: "Every namespace with its playing, pending and scheduled alerts, including due times.", inputSchema: { type: "object" } },
  { name: "delete_namespace", description: "Delete a whole namespace and every alert in it.", inputSchema: { type: "object", properties: { namespace: "string" }, required: ["namespace"] } },
  { name: "get_status", description: "What is playing, queued, stored and recently played; volume, quiet hours, background input and AirPlay receivers.", inputSchema: { type: "object" } },
  { name: "skip_current", description: "Skip the alert playing right now.", inputSchema: { type: "object" } },
  { name: "clear_alerts", description: "Clear every queued (not playing) alert.", inputSchema: { type: "object" } },
  { name: "set_volume", description: "Set the alert volume, 0.0 to 1.0.", inputSchema: { type: "object", properties: { volume: "number 0..1" }, required: ["volume"] } },
  { name: "play_background", description: "Play background music: an http(s) stream (fallback URLs allowed) or a looped local file, at an optional gain_db (default -6).", inputSchema: { type: "object", properties: { url: "http(s) stream URL", urls: "string[] — fallbacks in order", file: "local file, looped", gain_db: "number, e.g. -12 for quiet" } } },
  { name: "stop_background", description: "Stop the background music.", inputSchema: { type: "object" } },
];

export function McpDocs() {
  const [tools, setTools] = useState<McpTool[] | null>(null);
  const [fromLive, setFromLive] = useState(false);

  useEffect(() => {
    api<{ result: { tools: McpTool[] } }>("POST", "/mcp", {
      jsonrpc: "2.0",
      id: 1,
      method: "tools/list",
      params: {},
    })
      .then((res) => {
        setTools(res.result.tools);
        setFromLive(true);
      })
      .catch(() => setTools(FALLBACK));
  }, []);

  const shown = tools && tools.length ? tools : null;

  return (
    <>
      <Section title="What it is">
        <p className="text-sm text-muted">
          A <b className="font-semibold text-ink">Model Context Protocol</b> server so agents (Claude,
          scripts, anything speaking MCP) can drive unsonido with discovered tools instead of
          hand-rolled curl calls. Stateless Streamable HTTP on one route, protocol revision{" "}
          <code className="font-mono not-italic text-ink">2025-06-18</code>:{" "}
          <code className="font-mono not-italic text-ink">initialize</code>,{" "}
          <code className="font-mono not-italic text-ink">ping</code>,{" "}
          <code className="font-mono not-italic text-ink">tools/list</code> and{" "}
          <code className="font-mono not-italic text-ink">tools/call</code>; everything else is
          treated as notifications and acknowledged.
        </p>
        <div className="mt-3 flex flex-col gap-2">
          <CodeBlock code={TRANSPORT_CODE} />
          <CodeBlock code={CLIENT_CODE} />
          <CodeBlock code={EXAMPLE_CODE} />
        </div>
      </Section>

      <Section
        title="Tools"
        actions={
          <Tag kind={fromLive ? "ok" : "default"}>
            {fromLive ? "live from tools/list" : "reference copy"}
          </Tag>
        }
      >
        {shown ? (
          <p className="mb-3 text-xs text-muted">
            {shown.length} tools. Agents register the URL once and then call these by name.
          </p>
        ) : (
          <p className="text-sm text-muted">Loading tools…</p>
        )}
        <ul className="divide-y divide-line/70">
          {shown?.map((tool) => (
            <li key={tool.name} className="py-3 first:pt-0 last:pb-0">
              <div className="flex flex-wrap items-baseline gap-2">
                <code className="font-mono text-[13px] font-semibold text-ink">{tool.name}</code>
                <div className="ml-auto flex flex-wrap items-center gap-1">
                  {(tool.inputSchema?.required ?? []).map((arg) => (
                    <Tag key={arg} kind="accent" title="required">
                      {arg}
                    </Tag>
                  ))}
                  {Object.keys(tool.inputSchema?.properties ?? {})
                    .filter((arg) => !(tool.inputSchema?.required ?? []).includes(arg))
                    .map((arg) => (
                      <Tag key={arg} kind="muted" title="optional">
                        {arg}
                      </Tag>
                    ))}
                </div>
              </div>
              <p className="mt-1 text-sm text-muted">{tool.description}</p>
            </li>
          ))}
        </ul>
      </Section>
    </>
  );
}