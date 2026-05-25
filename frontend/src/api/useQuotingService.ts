// React hook for the quoting-service WS connection.
//
// Manages the WS lifecycle, exposes connection status, and provides
// helpers to send RFQ requests and consume responses.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  QuotingClient,
  nextRequestId,
  type ErrorPayload,
  type RfqRequestPayload,
  type RfqResponsePayload,
} from "./quoting";

export type RfqLogEntry = {
  id: string;
  ts: number;
  direction: "out" | "in";
  requestId: string;
  payload: RfqRequestPayload | RfqResponsePayload | ErrorPayload;
  kind: "request" | "response" | "error";
};

export type UseQuotingServiceOpts = {
  /** Override the WS URL (defaults to VITE_QUOTING_WS_URL / localhost). */
  url?: string;
  /** Auto-connect on mount. Defaults to true. */
  autoConnect?: boolean;
  /** Max log entries to keep. Defaults to 200. */
  maxLog?: number;
};

export function useQuotingService(opts: UseQuotingServiceOpts = {}) {
  const { url, autoConnect = true, maxLog = 200 } = opts;
  const [connected, setConnected] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [log, setLog] = useState<RfqLogEntry[]>([]);

  // Stable ref so callbacks never stale-capture React state.
  const clientRef = useRef<QuotingClient | null>(null);

  const pushLog = useCallback(
    (entry: Omit<RfqLogEntry, "id" | "ts">) => {
      setLog((prev) => {
        const next = [
          ...prev,
          { ...entry, id: `${Date.now()}-${Math.random()}`, ts: Date.now() },
        ];
        return next.length > maxLog ? next.slice(-maxLog) : next;
      });
    },
    [maxLog],
  );

  useEffect(() => {
    const client = new QuotingClient(
      {
        onOpen: () => setConnected(true),
        onClose: () => {
          setConnected(false);
          setSessionId(null);
        },
        onHelloAck: (sid) => setSessionId(sid),
        onRfqResponse: (requestId, payload) => {
          pushLog({ direction: "in", requestId, payload, kind: "response" });
        },
        onError: (requestId, payload) => {
          pushLog({
            direction: "in",
            requestId: requestId ?? "unknown",
            payload,
            kind: "error",
          });
        },
      },
      url,
    );
    clientRef.current = client;
    if (autoConnect) client.connect();
    return () => client.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url]);

  const sendRfq = useCallback(
    (payload: RfqRequestPayload): string => {
      const id = nextRequestId();
      pushLog({ direction: "out", requestId: id, payload, kind: "request" });
      clientRef.current?.sendRfq(payload, id);
      return id;
    },
    [pushLog],
  );

  const clearLog = useCallback(() => setLog([]), []);

  return { connected, sessionId, log, sendRfq, clearLog };
}
