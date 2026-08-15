// React hook around `quotingClient.sendRfq`.
//
// Debounces sends so a user dragging a strike tile or typing an amount
// doesn't fire one RFQ per render. Cancels in-flight responses when its
// inputs change so we never paint stale quotes.

import { useEffect, useState } from "react";
import { quotingClient } from "./quotingClient";
import type { BucketSpec, RfqQuoteEntry, Side } from "./quoting";

const DEBOUNCE_MS = 300;

export type RfqStatus = "idle" | "pending" | "ready" | "error";

export type UseRfqResult = {
  quotes: RfqQuoteEntry[];
  status: RfqStatus;
  /** Last error message (only set when `status === "error"`). */
  error: string | null;
  /** Force a fresh RFQ with the current inputs (e.g. after a quote lapses). */
  refresh: () => void;
};

export function useRfq(opts: {
  /** The economics to price. Null disables the hook. A strike whose bucket
   *  does not exist yet is perfectly quotable — that is the point. */
  spec: BucketSpec | null;
  /** u64 string of underlying smallest-units, or null to disable. */
  writeAmountRaw: string | null;
  side: Side;
  enabled: boolean;
}): UseRfqResult {
  const { spec, writeAmountRaw, side, enabled } = opts;
  // Stable identity for the effect: a fresh object every render would re-fire
  // the RFQ on every keystroke.
  const specKey = spec === null ? null : JSON.stringify(spec);
  const [quotes, setQuotes] = useState<RfqQuoteEntry[]>([]);
  const [status, setStatus] = useState<RfqStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  // Bumping this re-runs the effect with the same inputs, re-sending the RFQ.
  const [refreshTick, setRefreshTick] = useState(0);

  useEffect(() => {
    if (
      !enabled ||
      spec === null ||
      writeAmountRaw === null ||
      writeAmountRaw === "0"
    ) {
      setQuotes([]);
      setStatus("idle");
      setError(null);
      return;
    }
    let cancelled = false;
    setStatus("pending");
    setQuotes([]);
    setError(null);
    const handle = setTimeout(() => {
      quotingClient
        .sendRfq({ spec, writeAmount: writeAmountRaw, side })
        .then((entries) => {
          if (cancelled) return;
          console.log("[useRfq] rfq response", {
            spec,
            writeAmountRaw,
            side,
            count: entries.length,
            quotes: entries,
          });
          setQuotes(entries);
          setStatus("ready");
        })
        .catch((e: unknown) => {
          if (cancelled) return;
          const msg = e instanceof Error ? e.message : String(e);
          console.warn("[useRfq] rfq failed", msg);
          setQuotes([]);
          setStatus("error");
          setError(msg);
        });
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      clearTimeout(handle);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabled, specKey, writeAmountRaw, side, refreshTick]);

  return { quotes, status, error, refresh: () => setRefreshTick((n) => n + 1) };
}
