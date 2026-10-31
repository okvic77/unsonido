import { useEffect, useState } from "react";
import { api } from "../api";
import type { NamespaceInfo, StoredAlert, Status } from "../types";
import type { Notifier } from "../useStatus";
import { Btn, Section, Tag, relativeTime } from "./widgets";

/** kubectl-style stored alerts by namespace, collapsible, with live countdowns. */
export function Namespaces({
  status,
  notify,
  refresh,
}: {
  status: Status | null;
  notify: Notifier;
  refresh: () => void;
}) {
  const namespaces = status?.namespaces ?? [];
  return (
    <Section title="Namespaces" actions={<Tag>{namespaces.length}</Tag>}>
      {namespaces.length === 0 ? (
        <p className="text-sm text-muted">
          Empty. Alerts applied with a name wait here — at their time when scheduled — and
          self-destruct once they play.
        </p>
      ) : (
        <div className="flex flex-col gap-2">
          {namespaces.map((ns) => (
            <NamespaceCard key={ns.name} ns={ns} notify={notify} refresh={refresh} />
          ))}
        </div>
      )}
    </Section>
  );
}

function NamespaceCard({
  ns,
  notify,
  refresh,
}: {
  ns: NamespaceInfo;
  notify: Notifier;
  refresh: () => void;
}) {
  const [open, setOpen] = useState(true);
  return (
    <div className="rounded-xl border border-line">
      <div className="flex flex-wrap items-center gap-2 px-3 py-2.5">
        <button
          className="flex min-w-0 cursor-pointer items-center gap-2 rounded-lg py-0.5 text-left focus-visible:outline-2 focus-visible:outline-accent"
          onClick={() => setOpen(!open)}
          aria-expanded={open}
        >
          <span
            className={"h-3 w-3 shrink-0 text-muted transition-transform " + (open ? "rotate-90" : "")}
          >
            <svg viewBox="0 0 12 12" aria-hidden>
              <path d="M4 2.5 8 6l-4 3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </span>
          <span className="truncate text-sm font-semibold">{ns.name}</span>
        </button>
        <span className="ml-auto text-xs text-muted">
          {ns.alerts.length} stored
        </span>
        <Btn
          title="Delete this namespace and everything stored in it"
          onClick={() => {
            api("DELETE", "/namespaces/" + encodeURIComponent(ns.name)).then(refresh, notify.error);
          }}
        >
          Delete
        </Btn>
      </div>
      {open && (
        <ul className="divide-y divide-line/70 border-t border-line px-3 py-1">
          {ns.alerts.map((a) => (
            <StoredRow key={a.name} a={a} notify={notify} refresh={refresh} />
          ))}
        </ul>
      )}
    </div>
  );
}

function StoredRow({
  a,
  notify,
  refresh,
}: {
  a: StoredAlert;
  notify: Notifier;
  refresh: () => void;
}) {
  const icon = a.state === "playing" ? "▶" : a.state === "scheduled" ? "⏰" : "•";
  return (
    <li className="flex flex-wrap items-center gap-2 py-2 text-sm">
      <span className="w-4 shrink-0 text-center text-muted">{icon}</span>
      <span className="min-w-0 grow truncate">{a.label}</span>
      <Tag>{a.namespace}/{a.name}</Tag>
      {a.due_at && <Countdown dueAt={a.due_at} />}
      <Tag kind={a.priority === "critical" ? "critical" : "default"}>{a.priority}</Tag>
      <Btn
        onClick={() => {
          api(
            "DELETE",
            `/namespaces/${encodeURIComponent(a.namespace)}/alerts/${encodeURIComponent(a.name)}`,
          ).then(refresh, notify.error);
        }}
      >
        Delete
      </Btn>
    </li>
  );
}

/** "in 3 min", ticking once a second. */
function Countdown({ dueAt }: { dueAt: string }) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setTick((x) => x + 1), 1000);
    return () => clearInterval(t);
  }, []);
  return <Tag title={new Date(dueAt).toLocaleString()}>{relativeTime(dueAt)}</Tag>;
}