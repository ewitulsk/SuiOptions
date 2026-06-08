// React hook around `quotingClient.sendBulkView`.
//
// Fetches indicative (unsigned) premiums for every strike in a series at the
// current amount, to populate the tiles without firing a signable RFQ per
// tile. Debounces input changes and re-requests on an interval so the
// server-side stale-while-revalidate cache stays warm while a user lingers.

import { useEffect, useState } from "react";
import { quotingClient } from "./quotingClient";
import type { BulkViewPremium, Side } from "./quoting";

const DEBOUNCE_MS = 300;
// Re-request cadence. Slightly under the server's 30s cache TTL so an idle
// screen sees premiums refresh rather than going indefinitely stale.
const REFRESH_INTERVAL_MS = 20_000;

export type BulkViewStatus = "idle" | "pending" | "ready" | "error";

export type UseBulkViewResult = {
  /** bucket_id → averaged indicative premium entry. */
  premiums: Map<string, BulkViewPremium>;
  status: BulkViewStatus;
};

export function useBulkView(opts: {
  bucketIds: string[];
  /** u64 string of underlying smallest-units, or null to disable. */
  writeAmountRaw: string | null;
  side: Side;
  enabled: boolean;
}): UseBulkViewResult {
  const { bucketIds, writeAmountRaw, side, enabled } = opts;
  const [premiums, setPremiums] = useState<Map<string, BulkViewPremium>>(
    () => new Map(),
  );
  const [status, setStatus] = useState<BulkViewStatus>("idle");
  // Bumped on an interval to re-fire the request with the same inputs.
  const [tick, setTick] = useState(0);

  // Stable key so re-renders that produce an equal id list don't re-fire.
  // Sorted so a reordered-but-equal list is treated as unchanged.
  const idsKey = [...bucketIds].sort().join(",");

  useEffect(() => {
    if (!enabled) return;
    const h = setInterval(() => setTick((n) => n + 1), REFRESH_INTERVAL_MS);
    return () => clearInterval(h);
  }, [enabled]);

  useEffect(() => {
    if (
      !enabled ||
      idsKey === "" ||
      writeAmountRaw === null ||
      writeAmountRaw === "0"
    ) {
      setStatus("idle");
      return;
    }
    let cancelled = false;
    setStatus("pending");
    const ids = idsKey.split(",");
    const handle = setTimeout(() => {
      quotingClient
        .sendBulkView({ bucketIds: ids, writeAmount: writeAmountRaw, side })
        .then((entries) => {
          if (cancelled) return;
          const m = new Map<string, BulkViewPremium>();
          for (const e of entries) m.set(e.bucket_id, e);
          setPremiums(m);
          setStatus("ready");
        })
        .catch((e: unknown) => {
          if (cancelled) return;
          console.warn(
            "[useBulkView] failed",
            e instanceof Error ? e.message : String(e),
          );
          setStatus("error");
        });
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      clearTimeout(handle);
    };
  }, [enabled, idsKey, writeAmountRaw, side, tick]);

  return { premiums, status };
}
