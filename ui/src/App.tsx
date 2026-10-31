import { useCallback, useState } from "react";
import { AirPlay } from "./components/AirPlay";
import { AlertsQueue } from "./components/AlertsQueue";
import { Background } from "./components/Background";
import { Namespaces } from "./components/Namespaces";
import { Output } from "./components/Output";
import { SendAlert } from "./components/SendAlert";
import { TokenBox } from "./components/TokenBox";
import { useToasts } from "./components/Toast";
import { loadToken } from "./api";
import { useStatus, type ConnectionState } from "./useStatus";

export function App() {
  const notify = useToasts();
  // Bumping this remounts SendAlert so it re-fetches TTS providers with new auth.
  const [tokenVersion, setTokenVersion] = useState(() => loadToken());
  const { status, connection, refresh } = useStatus(notify);

  const refreshProviders = useCallback(() => {
    setTokenVersion(String(Date.now()));
  }, []);

  return (
    <div className="mx-auto flex max-w-2xl flex-col gap-3 px-4 pb-20 pt-6 sm:gap-4">
      <Header connection={connection} />
      <Output status={status} notify={notify} />
      <AirPlay status={status} notify={notify} refresh={refresh} />
      <Background status={status} notify={notify} refresh={refresh} />
      <SendAlert key={tokenVersion} notify={notify} refresh={refresh} />
      <Namespaces status={status} notify={notify} refresh={refresh} />
      <AlertsQueue status={status} notify={notify} refresh={refresh} />
      <TokenBox notify={notify} onSaved={refreshProviders} />
    </div>
  );
}

function Header({ connection }: { connection: ConnectionState }) {
  const dot =
    connection === "ok"
      ? "bg-ok"
      : connection === "error"
        ? "bg-warn"
        : "bg-line";
  const label =
    connection === "ok"
      ? "server reachable"
      : connection === "error"
        ? "server unreachable"
        : "connecting…";
  return (
    <header className="flex items-center gap-3 pt-1">
      <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-ink text-card">
        <svg viewBox="0 0 24 24" className="h-5 w-5" aria-hidden>
          <g stroke="currentColor" strokeWidth="2" strokeLinecap="round">
            <line x1="4" y1="10" x2="4" y2="14" />
            <line x1="9" y1="7" x2="9" y2="17" />
            <line x1="14" y1="9" x2="14" y2="15" />
            <line x1="19" y1="11" x2="19" y2="13" />
          </g>
        </svg>
      </div>
      <div className="min-w-0">
        <h1 className="text-lg leading-tight font-semibold">unsonido</h1>
        <p className="text-xs text-muted">one room, one mix</p>
      </div>
      <span
        className={"ml-auto inline-flex items-center gap-2 text-xs text-muted"}
        title={label}
      >
        <span className={"h-2 w-2 rounded-full " + dot} />
        {connection === "error" ? "unreachable" : "live"}
      </span>
    </header>
  );
}