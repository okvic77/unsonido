import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

interface ToastMessage {
  id: number;
  kind: "success" | "error";
  text: string;
}

const ToastContext = createContext<{
  success: (message: string) => void;
  error: (err: unknown) => void;
}>({ success: () => {}, error: () => {} });

/** Toasts: errors and confirmations, auto-dismissing, bottom center. */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<ToastMessage[]>([]);
  const nextId = useRef(0);

  const push = useCallback((kind: ToastMessage["kind"], text: string) => {
    const id = nextId.current++;
    setToasts((t) => [...t.slice(-2), { id, kind, text }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 5000);
  }, []);

  const success = useCallback(
    (message: string) => push("success", message),
    [push],
  );

  const error = useCallback(
    (err: unknown) => {
      const message =
        err instanceof Error ? err.message : String(err ?? "unknown error");
      push("error", message);
    },
    [push],
  );

  const value = useMemo(() => ({ success, error }), [success, error]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div className="pointer-events-none fixed inset-x-0 bottom-5 z-50 flex flex-col items-center gap-2 px-4">
        {toasts.map((t) => (
          <div
            key={t.id}
            role={t.kind === "error" ? "alert" : "status"}
            className={
              "pointer-events-auto max-w-md rounded-xl px-4 py-2.5 text-sm shadow-lg " +
              (t.kind === "error"
                ? "bg-warn text-white"
                : "bg-ink text-card")
            }
          >
            {t.text}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToasts() {
  return useContext(ToastContext);
}