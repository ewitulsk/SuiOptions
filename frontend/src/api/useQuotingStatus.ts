import { useEffect, useSyncExternalStore } from "react";
import { quotingClient, type QuotingStatus } from "./quotingClient";

// Reflects the live quoting-service WS connection. Proactively opens the
// socket on mount so the status is meaningful even without RFQ traffic.
export function useQuotingStatus(): QuotingStatus {
  useEffect(() => {
    void quotingClient.connect_().catch(() => undefined);
  }, []);
  return useSyncExternalStore(
    quotingClient.subscribe,
    quotingClient.getStatus,
    quotingClient.getStatus,
  );
}
