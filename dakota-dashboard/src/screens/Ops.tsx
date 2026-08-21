import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import * as api from "../api/dakota";
import { ErrorBox, Panel } from "../components/ui";
import { useAuthed } from "../state/session";

/** Operational plumbing an admin occasionally needs to touch. */
export default function Ops() {
  const { token } = useAuthed();
  const targets = useQuery({ queryKey: ["webhooks"], queryFn: () => api.listWebhooks(token) });

  const [error, setError] = useState<unknown>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (fn: () => Promise<string>) => {
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      setNote(await fn());
      await targets.refetch();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const registered = targets.data?.data ?? [];

  return (
    <>
      <h2>Ops</h2>
      <ErrorBox error={error} />
      {note && <div className="success">{note}</div>}

      <Panel
        title="Webhook delivery"
        hint="Dakota pushes events here as transfers settle. Registration is manual so a restart does not churn targets — but nothing lands in the activity feed until it is done once."
      >
        <p className="muted">
          {registered.length
            ? `${registered.length} target(s) registered.`
            : "No targets registered — the activity feed will stay empty."}
        </p>
        <button
          disabled={busy}
          onClick={() =>
            void run(async () => {
              const r = await api.registerWebhook(token);
              return `Registered ${r.url}`;
            })
          }
        >
          Register this deployment
        </button>
      </Panel>

      <Panel
        title="Resync the ledger"
        hint="Replays Dakota's event log through the same extractor the webhook uses. Safe to run repeatedly — events are keyed by id, so replays cannot double-count."
      >
        <button
          className="secondary"
          disabled={busy}
          onClick={() =>
            void run(async () => {
              const r = await api.resync(token);
              return `Scanned ${r.scanned}, inserted ${r.inserted} new.`;
            })
          }
        >
          Resync from Dakota
        </button>
        <p className="muted" style={{ marginTop: 10 }}>
          Use this after registering the webhook late, or after downtime longer than Dakota's
          48-hour retry window.
        </p>
      </Panel>
    </>
  );
}
