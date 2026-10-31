import { useState } from "react";
import { api } from "../api";
import type { BackgroundStatus, Status } from "../types";
import type { Notifier } from "../useStatus";
import { stationGroups, STATIONS, type Station } from "../data/radio";
import { Btn, Dot, Section, Tag, inputCls } from "./widgets";

/** Background radio/Icecast stream: status, URL list (one per line), play/stop,
 * and suggested streams that add themselves to the list with a click. */
export function Background({
  status,
  notify,
  refresh,
}: {
  status: Status | null;
  notify: Notifier;
  refresh: () => void;
}) {
  const bg: BackgroundStatus | null = status?.background ?? null;
  const [draft, setDraft] = useState<string | null>(null);

  // Follow the server while the user hasn't got the textarea in hand.
  const urls: string[] = [];
  if (bg?.url) urls.push(bg.url);
  if (bg?.urls) urls.push(...bg.urls);
  const text = draft ?? (urls.length ? urls.join("\n") : "");

  const lines = text.split("\n").map((u) => u.trim());
  const which =
    bg && bg.current_url && urls.length > 1
      ? urls.indexOf(bg.current_url) === 0
        ? "primary"
        : `fallback ${urls.indexOf(bg.current_url)}`
      : null;

  /** Click a chip: add the station's URL to the list (unless it is already there). */
  const addStation = (s: Station) => {
    if (lines.includes(s.url)) {
      notify.success(`${s.name} is already in the list`);
      return;
    }
    setDraft([...lines.filter(Boolean), s.url].join("\n"));
    notify.success(`${s.name} added to the list`);
  };

  /** The ▶ inside a chip: tune in to just that station right away. */
  const tuneIn = (s: Station) => {
    api("PUT", "/background", { urls: [s.url] })
      .then(() => {
        notify.success(`Tuned in to ${s.name}`);
        setDraft(null);
        refresh();
      }, notify.error);
  };

  return (
    <Section title="Background">
      <p className="mb-3 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-muted">
        {bg ? (
          <>
            <Dot on={bg.playing} />
            <span className="max-w-full truncate font-mono text-xs">
              {bg.current_url ?? bg.url ?? bg.file}
            </span>
            {which && <Tag>{which}</Tag>}
            <span>{bg.state}</span>
            {bg.title && <b className="text-ink">{bg.title}</b>}
          </>
        ) : (
          "Nothing playing"
        )}
      </p>
      <textarea
        rows={3}
        value={text}
        placeholder={
          "https://… Icecast / radio stream URL\nhttps://… fallback, used if the first one fails\n…one URL per line"
        }
        onBlur={() => {
          // Dropping a draft that matches the server keeps the box in sync.
          const normalized = lines.filter(Boolean);
          if (draft !== null && normalized.join("\n") === urls.join("\n")) setDraft(null);
        }}
        onChange={(e) => setDraft(e.target.value)}
        className={inputCls + " w-full font-mono text-xs"}
      />
      <div className="mt-2.5 flex items-center gap-2">
        <span className="min-w-0 grow text-xs text-muted">
          One URL per line; the ones after the first are fallbacks.
        </span>
        <Btn
          variant="primary"
          onClick={() => {
            const list = lines.filter(Boolean);
            api("PUT", "/background", { urls: list })
              .then(() => {
                notify.success("Background playing");
                setDraft(null);
                refresh();
              }, notify.error);
          }}
        >
          Play
        </Btn>
        <Btn
          onClick={() => {
            api("DELETE", "/background").then(() => {
              notify.success("Background stopped");
              setDraft(null);
              refresh();
            }, notify.error);
          }}
        >
          Stop
        </Btn>
      </div>

      <Suggestions lines={lines} onAdd={addStation} onTuneIn={tuneIn} />
    </Section>
  );
}

/** Click-to-curate station chips, grouped by mood; ▶ tunes in immediately. */
function Suggestions({
  lines,
  onAdd,
  onTuneIn,
}: {
  lines: string[];
  onAdd: (s: Station) => void;
  onTuneIn: (s: Station) => void;
}) {
  return (
    <div className="mt-4 border-t border-line/70 pt-3.5">
      <h3 className="mb-2.5 text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">
        Suggested streams
      </h3>
      {stationGroups().map((group) => (
        <div key={group} className="mb-2.5 last:mb-0">
          <p className="mb-1.5 text-[11px] text-muted/80">{group}</p>
          <div className="flex flex-wrap gap-1.5">
            {STATIONS.filter((s) => s.group === group).map((s) => {
              const listed = lines.includes(s.url);
              return (
                <span
                  key={s.url}
                  className={
                    "inline-flex items-center overflow-hidden rounded-full border transition-colors " +
                    (listed ? "border-ok/50 bg-ok/10" : "border-line bg-paper hover:border-accent")
                  }
                >
                  <button
                    type="button"
                    onClick={() => onAdd(s)}
                    title={listed ? `${s.name} — already in the list` : `Add ${s.name} to the list`}
                    className={
                      "cursor-pointer px-3 py-1 text-xs whitespace-nowrap focus-visible:outline-2 focus-visible:outline-offset-0 focus-visible:outline-accent " +
                      (listed ? "text-ok" : "text-ink hover:text-accent")
                    }
                  >
                    {listed && <span aria-hidden>✓ </span>}
                    {s.name}
                  </button>
                  <button
                    type="button"
                    aria-label={`Tune in to ${s.name} now`}
                    title="Tune in now — plays only this station, replacing the list"
                    onClick={() => onTuneIn(s)}
                    className="cursor-pointer border-l border-line px-2 py-1 text-[10px] text-muted transition-colors hover:bg-accent hover:text-white focus-visible:outline-2 focus-visible:outline-offset-0 focus-visible:outline-accent"
                  >
                    ▶
                  </button>
                </span>
              );
            })}
          </div>
        </div>
      ))}
      <p className="mt-3 text-[11px] text-muted/80">
        SomaFM and Radio Paradise are listener-supported independent radio. Thousands more
        stations are browsable at <span className="font-mono">radio-browser.info</span> — paste
        any direct stream URL above.
      </p>
    </div>
  );
}