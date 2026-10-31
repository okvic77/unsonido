import { useState } from "react";
import { loadToken, setToken } from "../api";
import type { Notifier } from "../useStatus";
import { Btn, Section, inputCls } from "./widgets";

/** Access token (Bearer) for the API; only needed when server.token is configured. */
export function TokenBox({
  notify,
  onSaved,
}: {
  notify: Notifier;
  onSaved: () => void;
}) {
  const [token, setTokenState] = useState<string>(() => loadToken());
  return (
    <Section title="Access token">
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          setToken(token);
          notify.success("Token saved");
          onSaved();
        }}
      >
        <input
          type="password"
          value={token}
          placeholder="Only needed if server.token is set"
          onChange={(e) => setTokenState(e.target.value)}
          className={inputCls + " min-w-48 grow"}
        />
        <Btn type="submit" variant="primary">
          Save
        </Btn>
      </form>
    </Section>
  );
}