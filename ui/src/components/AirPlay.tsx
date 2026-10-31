import { api } from "../api";
import type { ReceiverStatus, Status } from "../types";
import type { Notifier } from "../useStatus";
import { Btn, Dot, Section, Slider, Tag, nowPlayingText } from "./widgets";

/** AirPlay receivers: connection state, track, per-speaker trim and mute. */
export function AirPlay({
  status,
  notify,
  refresh,
}: {
  status: Status | null;
  notify: Notifier;
  refresh: () => void;
}) {
  const receivers = status?.airplay ?? [];
  return (
    <Section title="AirPlay" actions={<Tag>{receivers.length}</Tag>}>
      {receivers.length === 0 ? (
        <p className="text-sm text-muted">No speakers yet. Pick “Unsonido …” in an AirPlay menu.</p>
      ) : (
        <ul className="divide-y divide-line/70">
          {receivers.map((r) => (
            <SpeakerRow key={r.name} r={r} notify={notify} refresh={refresh} />
          ))}
        </ul>
      )}
    </Section>
  );
}

function SpeakerRow({
  r,
  notify,
  refresh,
}: {
  r: ReceiverStatus;
  notify: Notifier;
  refresh: () => void;
}) {
  const base = "/airplay/" + encodeURIComponent(r.name);
  const track = nowPlayingText(r.now_playing ?? {});
  return (
    <li className="flex flex-col gap-2.5 py-3 first:pt-1 last:pb-1 sm:flex-row sm:items-center sm:gap-4">
      <div className="min-w-0 grow basis-56">
        <div className="flex items-center gap-2">
          <Dot on={r.playing} />
          <span className="min-w-0 flex-nowrap truncate text-sm font-semibold">{r.name}</span>
          <span className="truncate text-xs text-muted">
            · {r.client ?? "available"}
          </span>
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-1.5 pl-4 text-xs text-muted">
          {track && <span className="truncate">♪ {track}</span>}
          {r.paused_for_alert && <Tag kind="critical">paused for alert</Tag>}
          {r.client && r.volume_synced === false && (
            <Tag title="The device did not accept the volume change; it applies here only">
              volume not synced to device
            </Tag>
          )}
        </div>
      </div>
      <div className="flex items-center gap-2 sm:w-[24rem] sm:shrink-0">
        <Btn
          onClick={() => {
            api("PUT", base, { muted: !r.muted }).then(refresh, notify.error);
          }}
          aria-pressed={r.muted}
          title={r.muted ? "Unmute this speaker" : "Mute this speaker"}
        >
          {r.muted ? "Unmute" : "Mute"}
        </Btn>
        <Slider
          value={r.volume}
          className="grow"
          onCommit={(v) => {
            api("PUT", base, { volume: v }).then(
              () => notify.success(`Volume ${Math.round(v * 100)}% on ${r.name}`),
              notify.error,
            );
          }}
        />
        <Btn
          disabled={!r.client}
          title={r.client ? "Disconnect the sender" : "Nothing is connected"}
          onClick={() => {
            api("DELETE", base + "/session").then(refresh, notify.error);
          }}
        >
          Disconnect
        </Btn>
      </div>
    </li>
  );
}