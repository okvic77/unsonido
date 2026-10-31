import { useState } from "react";
import { api } from "../api";
import type { BackgroundStatus, Status } from "../types";
import type { Notifier } from "../useStatus";
import { Btn, Dot, Section, Tag, inputCls } from "./widgets";

/** Background radio/Icecast stream: status, URL list (one per line), play/stop. */
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

  const which =
    bg && bg.current_url && urls.length > 1
      ? urls.indexOf(bg.current_url) === 0
        ? "primary"
        : `fallback ${urls.indexOf(bg.current_url)}`
      : null;

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
          const normalized = draft?.split("\n").map((u) => u.trim()).filter(Boolean);
          if (normalized && normalized.join("\n") === urls.join("\n")) setDraft(null);
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
            const list = text.split("\n").map((u) => u.trim()).filter(Boolean);
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
    </Section>
  );
}