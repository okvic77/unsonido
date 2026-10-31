import { useEffect, useState } from "react";
import { api } from "../api";
import type { Outcome, Priority, ProviderInfo } from "../types";
import type { Notifier } from "../useStatus";
import { Btn, Section, inputCls } from "./widgets";

/** Speak an alert now, or apply it kubectl-style into a namespace, optionally scheduled. */
export function SendAlert({
  notify,
  refresh,
}: {
  notify: Notifier;
  refresh: () => void;
}) {
  const [text, setText] = useState("");
  const [priority, setPriority] = useState<Priority>("notification");
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [provider, setProvider] = useState("");
  const [namespace, setNamespace] = useState("");
  const [name, setName] = useState("");
  const [at, setAt] = useState("");
  const [sending, setSending] = useState(false);

  useEffect(() => {
    const def = providers.find((p) => p.default);
    if (def && !provider) setProvider(def.name);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [providers]);

  useEffect(() => {
    api<ProviderInfo[]>("GET", "/tts/providers")
      .then(setProviders)
      .catch(notify.error);
  }, [notify]);

  async function send() {
    if (sending) return;
    if (at && !name) {
      notify.error(new Error("scheduling needs a name — the alert is stored until its time"));
      return;
    }
    setSending(true);
    const body: Record<string, unknown> = { text, priority };
    if (provider) body.provider = provider;
    try {
      const outcome = name
        ? await api<Outcome>(
            "PUT",
            `/namespaces/${encodeURIComponent(namespace || "default")}/alerts/${encodeURIComponent(name)}`,
            at ? { ...body, at } : body,
          )
        : await api<Outcome>("POST", "/alerts", body);
      notifyOutcome(outcome, notify);
      setText("");
      setName("");
      setAt("");
      refresh();
    } catch (err) {
      notify.error(err);
    } finally {
      setSending(false);
    }
  }

  return (
    <Section title="Send alert">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
        className="flex flex-col gap-2.5"
      >
        <div className="flex items-stretch gap-2">
          <input
            type="text"
            value={text}
            placeholder="Text to speak"
            onChange={(e) => setText(e.target.value)}
            className={inputCls + " min-w-0 grow"}
          />
          <PrioritySwitch value={priority} onChange={setPriority} />
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {providers.length > 1 && (
            <select
              value={provider}
              onChange={(e) => setProvider(e.target.value)}
              className={inputCls + " w-auto"}
              aria-label="TTS provider"
            >
              {providers.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name} ({p.kind}){p.default ? " ★" : ""}
                </option>
              ))}
            </select>
          )}
          <div className="grow" />
          <Btn type="submit" variant="primary" disabled={!text.trim() || sending}>
            Send
          </Btn>
        </div>
        <details className="group text-sm">
          <summary className="w-fit cursor-pointer list-none text-xs text-muted hover:text-ink">
            Namespace, name & schedule ▾
          </summary>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <input
              type="text"
              value={namespace}
              placeholder="namespace (default)"
              onChange={(e) => setNamespace(e.target.value)}
              className={inputCls + " w-44"}
            />
            <input
              type="text"
              value={name}
              placeholder="name — stored & replaceable in the namespace"
              onChange={(e) => setName(e.target.value)}
              className={inputCls + " min-w-40 grow"}
            />
            <input
              type="datetime-local"
              value={at}
              title="Play at this time instead of now"
              onChange={(e) => setAt(e.target.value)}
              className={inputCls + " w-52"}
            />
          </div>
          <p className="mt-1.5 text-xs text-muted">
            With a name the alert is applied to a namespace: replace or delete it until it plays;
            once played it self-destructs. With a time it waits until then. Without a name it is
            sent anonymously.
          </p>
        </details>
      </form>
    </Section>
  );
}

function notifyOutcome(outcome: Outcome, notify: Notifier) {
  switch (outcome.status) {
    case "queued":
      notify.success("Alert queued");
      break;
    case "scheduled":
      notify.success(
        "Scheduled for " +
          new Date(outcome.due_at).toLocaleString([], {
            dateStyle: "medium",
            timeStyle: "short",
          }),
      );
      break;
    case "unchanged":
      notify.success("Already stored in the namespace — nothing changed");
      break;
    case "suppressed":
      notify.error(new Error(`Suppressed: ${outcome.reason}`));
      break;
  }
}

/** Two-button segmented control instead of a <select>. */
function PrioritySwitch({
  value,
  onChange,
}: {
  value: Priority;
  onChange: (p: Priority) => void;
}) {
  const options: { key: Priority; label: string }[] = [
    { key: "notification", label: "Notify" },
    { key: "critical", label: "Critical" },
  ];
  return (
    <div
      role="radiogroup"
      aria-label="Priority"
      className="inline-flex shrink-0 items-center rounded-xl border border-line bg-paper p-1"
    >
      {options.map((o) => (
        <button
          key={o.key}
          type="button"
          role="radio"
          aria-checked={value === o.key}
          onClick={() => onChange(o.key)}
          className={
            "cursor-pointer rounded-lg px-3 py-1.5 text-xs font-medium transition-colors focus-visible:outline-2 focus-visible:outline-accent " +
            (value === o.key
              ? o.key === "critical"
                ? "bg-warn text-white"
                : "bg-accent text-white"
              : "text-muted hover:text-ink")
          }
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}