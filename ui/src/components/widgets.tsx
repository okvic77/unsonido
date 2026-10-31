import {
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";

/** Shared building blocks for the control page. */

export const btn =
  "inline-flex cursor-pointer items-center justify-center gap-1.5 rounded-xl border border-line bg-paper px-3.5 py-2 text-sm font-medium text-ink transition-colors hover:bg-line/50 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:pointer-events-none disabled:opacity-40";
export const btnPrimary =
  btn + " border-accent bg-accent text-white hover:opacity-90";
export const inputCls =
  "rounded-xl border border-line bg-paper px-3 py-2 text-sm text-ink placeholder:text-muted/60 focus-visible:outline-2 focus-visible:outline-offset-0 focus-visible:outline-accent";

export function Section({
  title,
  actions,
  children,
}: {
  title: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="rounded-2xl border border-line bg-card p-4 shadow-sm sm:p-5">
      <div className="mb-3.5 flex items-center gap-2">
        <h2 className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted">
          {title}
        </h2>
        {actions && <div className="ml-auto flex items-center gap-2">{actions}</div>}
      </div>
      {children}
    </section>
  );
}

export function Tag({
  kind = "default",
  title,
  children,
}: {
  kind?: "default" | "critical" | "ok";
  title?: string;
  children: ReactNode;
}) {
  const cls = {
    default: "border-line text-muted",
    critical: "border-warn/60 text-warn",
    ok: "border-ok/50 text-ok",
  }[kind];
  return (
    <span
      title={title}
      className={
        "inline-flex shrink-0 items-center rounded-full border px-2 py-0.5 text-[11px] leading-none whitespace-nowrap " +
        cls
      }
    >
      {children}
    </span>
  );
}

export function Dot({ on, tone = "ok" }: { on?: boolean; tone?: "ok" | "warn" }) {
  return (
    <span
      className={
        "inline-block h-2 w-2 shrink-0 rounded-full " +
        (on ? (tone === "warn" ? "bg-warn" : "bg-ok") : "bg-line")
      }
    />
  );
}

/**
 * A range input that does not fight the server: while the pointer is down (or a
 * key is held) it shows its own pending value and ignores external updates,
 * committing once on release — the same contract the old page implemented by hand.
 */
export function Slider({
  value,
  onPreview,
  onCommit,
  className = "",
  ...props
}: {
  value: number;
  onPreview?: (v: number) => void;
  onCommit: (v: number) => void;
} & Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange">) {
  const [pending, setPending] = useState<number | null>(null);
  const shown = pending ?? value;

  const commit = () => {
    setPending((p) => {
      if (p !== null) onCommit(p);
      return null;
    });
  };

  return (
    <input
      type="range"
      min={0}
      max={1}
      step={0.01}
      aria-label="Volume"
      className={"w-full " + className}
      {...props}
      value={shown}
      onPointerDown={() => setPending(value)}
      onPointerCancel={() => setPending(null)}
      onLostPointerCapture={() => commit()}
      onPointerUp={() => commit()}
      onKeyUp={() => commit()}
      onBlur={() => {
        if (pending !== null) commit();
      }}
      onChange={(e) => {
        // In React, onChange fires for every drag tick (it is the input
        // event); the pointer/keyboard handlers above commit on release.
        const v = Number(e.currentTarget.value);
        setPending(v);
        onPreview?.(v);
      }}
    />
  );
}

/** Time formatting shared by queue, history and scheduled alerts. */

export function fmtClock(iso: string): string {
  return new Date(iso).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** "in 3 min" / "in 42s" / "at 14:30" / "Oct 31, 08:00" / "due now". */
export function relativeTime(iso: string): string {
  const diff = new Date(iso).getTime() - Date.now();
  if (diff > 0) {
    const s = Math.round(diff / 1000);
    if (s < 60) return `in ${s}s`;
    if (s < 3600) return `in ${Math.round(s / 60)} min`;
    const due = new Date(iso);
    const today = new Date().toDateString() === due.toDateString();
    return today
      ? `at ${due.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`
      : due.toLocaleDateString([], { month: "short", day: "numeric" }) +
          ", " +
          due.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return "due now";
}

export function nowPlayingText(np: {
  title?: string | null;
  artist?: string | null;
}): string {
  return [np.title, np.artist].filter(Boolean).join(" — ");
}

/** Small inline icon set (stroke-based, currentColor). */
export function Icon({
  name,
  className = "h-4 w-4",
}: {
  name: "speaker" | "radio" | "bell" | "layers" | "history" | "key";
  className?: string;
}) {
  const paths: Record<string, ReactNode> = {
    speaker: (
      <>
        <path d="M4 9v6h4l5 4V5L8 9H4Z" />
        <path d="M16.5 8.5a5 5 0 0 1 0 7" />
        <path d="M19 6a8.5 8.5 0 0 1 0 12" />
      </>
    ),
    radio: (
      <>
        <circle cx="12" cy="12" r="2" />
        <path d="M7 7a7 7 0 0 0 0 10M17 7a7 7 0 0 1 0 10M4.5 4.5a10.5 10.5 0 0 0 0 15M19.5 4.5a10.5 10.5 0 0 1 0 15" />
      </>
    ),
    bell: (
      <>
        <path d="M6 9a6 6 0 0 1 12 0c0 5 2 6 2 6H4s2-1 2-6" />
        <path d="M10 20a2 2 0 0 0 4 0" />
      </>
    ),
    layers: (
      <>
        <path d="M12 3 3 8l9 5 9-5-9-5Z" />
        <path d="m3 13 9 5 9-5" />
      </>
    ),
    history: (
      <>
        <path d="M3 12a9 9 0 1 0 3-6.7L4 7" />
        <path d="M3 3v4h4" />
        <path d="M12 7v5l3 3" />
      </>
    ),
    key: (
      <>
        <circle cx="8" cy="15" r="4" />
        <path d="m11 12 9-9M17 6l3 3M21 4l-2.5 2.5" />
      </>
    ),
  };
  return (
    <svg
      viewBox="0 0 24 24"
      className={className}
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      {paths[name]}
    </svg>
  );
}

/** A button; variant = default / primary / primary-danger for destructive primary actions. */
export function Btn({
  variant = "default",
  className = "",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "default" | "primary";
}) {
  return (
    <button
      className={(variant === "primary" ? btnPrimary : btn) + " " + className}
      {...props}
    />
  );
}