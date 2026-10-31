import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";
import type { Status } from "./types";

export type ConnectionState = "connecting" | "ok" | "error";

export interface Notifier {
  success: (message: string) => void;
  error: (err: unknown) => void;
}

/**
 * Polls GET /status every 1.5s while the tab is visible, pausing in the
 * background and refreshing immediately on focus. The last poll's result
 * drives the header's connection dot; failures are toasted once per burst.
 */
export function useStatus(notify: Notifier): {
  status: Status | null;
  connection: ConnectionState;
  refresh: () => void;
} {
  const [status, setStatus] = useState<Status | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("connecting");
  const errored = useRef(false);

  const refresh = useCallback(() => {
    api<Status>("GET", "/status")
      .then((s) => {
        errored.current = false;
        setStatus(s);
        setConnection("ok");
      })
      .catch((err) => {
        setConnection("error");
        if (!errored.current) {
          errored.current = true;
          notify.error(err);
        }
      });
  }, [notify]);

  useEffect(() => {
    refresh();
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") refresh();
    }, 1500);
    const wake = () => {
      if (document.visibilityState === "visible") refresh();
    };
    document.addEventListener("visibilitychange", wake);
    window.addEventListener("focus", wake);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", wake);
      window.removeEventListener("focus", wake);
    };
  }, [refresh]);

  return { status, connection, refresh };
}