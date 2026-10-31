import { api } from "../api";
import type { AlertInfo, Status } from "../types";
import type { Notifier } from "../useStatus";
import { Btn, Section, Tag, fmtClock } from "./widgets";

/** What is playing now, what is queued, and the recent history. */
export function AlertsQueue({
  status,
  notify,
  refresh,
}: {
  status: Status | null;
  notify: Notifier;
  refresh: () => void;
}) {
  const named = (a: AlertInfo) =>
    a.namespace ? ` ${a.namespace}/${a.name}` : "";

  return (
    <Section
      title="Alerts"
      actions={
        <>
          <Btn
            title="Skip the alert that is playing"
            onClick={() => api("DELETE", "/alerts/current").then(refresh, notify.error)}
          >
            Skip current
          </Btn>
          <Btn
            title="Skip the current alert and empty the queue"
            onClick={() => api("DELETE", "/alerts").then(refresh, notify.error)}
          >
            Clear queue
          </Btn>
        </>
      }
    >
      <ul className="divide-y divide-line/70 text-sm">
        {status?.current && (
          <li className="flex flex-wrap items-center gap-2 py-2.5 font-medium">
            <span className="w-16 shrink-0 text-xs tracking-wide text-ok uppercase">now</span>
            <span className="min-w-0 grow truncate">▶ {status.current.label}</span>
            {status.current.namespace && <Tag>{named(status.current).trim()}</Tag>}
            <Tag kind={status.current.priority === "critical" ? "critical" : "ok"}>
              {status.current.priority}
            </Tag>
          </li>
        )}
        {status?.queue.map((a, i) => (
          <li key={a.id} className="flex flex-wrap items-center gap-2 py-2.5">
            <span className="w-16 shrink-0 text-xs tracking-wide text-muted uppercase">
              #{i + 1}
            </span>
            <span className="min-w-0 grow truncate text-muted">{a.label}</span>
            {a.namespace && <Tag>{named(a).trim()}</Tag>}
            <Tag kind={a.priority === "critical" ? "critical" : "default"}>{a.priority}</Tag>
          </li>
        ))}
        {status?.history.slice(0, 8).map((h) => (
          <li key={h.id + h.at} className="flex flex-wrap items-center gap-2 py-2.5">
            <span className="w-16 shrink-0 font-mono text-xs text-muted">{fmtClock(h.at)}</span>
            <span className="min-w-0 grow truncate text-muted">{h.label}</span>
            {h.namespace && <Tag>{named(h).trim()}</Tag>}
            <Tag>{h.result}</Tag>
          </li>
        ))}
        {!status?.current && !status?.queue.length && !status?.history.length && (
          <li className="py-2.5 text-muted">No alerts yet</li>
        )}
      </ul>
    </Section>
  );
}