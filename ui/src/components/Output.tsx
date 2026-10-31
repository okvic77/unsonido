import { useEffect, useState } from "react";
import { api } from "../api";
import type { Status } from "../types";
import type { Notifier } from "../useStatus";
import { Icon, Section, Slider } from "./widgets";

/** Master output volume, in percent, with the sample rate from /status. */
export function Output({
  status,
  notify,
}: {
  status: Status | null;
  notify: Notifier;
}) {
  // Non-null while the user's last committed value awaits the next poll; it
  // clears once a poll confirms it (±0.5%) so the label follows the server again.
  const [shown, setShown] = useState<number | null>(null);
  useEffect(() => {
    if (shown !== null && status && Math.abs(shown - status.volume) < 0.005) setShown(null);
  }, [shown, status?.volume]);

  const volume = shown ?? status?.volume ?? 0;
  const khz = status ? (status.sample_rate / 1000).toFixed(1) : null;

  return (
    <Section
      title="Output"
      actions={
        <span className="flex items-center gap-1.5 text-xs text-muted">
          <Icon name="speaker" className="h-3.5 w-3.5" />
          {khz ? `${khz} kHz` : "…"}
        </span>
      }
    >
      <div className="flex items-center gap-3">
        <Slider
          value={status?.volume ?? 0}
          className="h-6 grow"
          onPreview={(v) => setShown(v)}
          onCommit={(v) => {
            setShown(v);
            api("PUT", "/volume", { volume: v })
              .then(() => notify.success(`Volume ${Math.round(v * 100)}%`), notify.error)
              .finally(() => setShown(null));
          }}
        />
        <span className="w-12 text-right font-mono text-sm tabular-nums text-muted">
          {Math.round(volume * 100)}%
        </span>
      </div>
    </Section>
  );
}