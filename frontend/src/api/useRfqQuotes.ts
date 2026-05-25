// Hook that fires an RFQ for every bucket in a series on a 1-minute interval
// and exposes the latest quote per bucket_id.
//
// Used by the Earn screen to replace mocked premiums with real quotes from
// the quoting service.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  QuotingClient,
  nextRequestId,
  type RfqQuoteEntry,
  type RfqResponsePayload,
} from "./quoting";
import type { Series } from "./client";

const RFQ_INTERVAL_MS = 60_000;
const DEFAULT_WRITE_AMOUNT = "100000000"; // 1 unit (8 decimals)

export type BucketQuote = {
  bucketId: string;
  quotes: RfqQuoteEntry[];
  bestPremiumRaw: string;
  updatedAt: number;
};

export type UseRfqQuotesOpts = {
  series: Series | null;
  side: "writer" | "trader";
  writeAmount?: string;
  url?: string;
  enabled?: boolean;
};

export function useRfqQuotes(opts: UseRfqQuotesOpts) {
  const {
    series,
    side,
    writeAmount = DEFAULT_WRITE_AMOUNT,
    url,
    enabled = true,
  } = opts;

  const [connected, setConnected] = useState(false);
  const [quotesByBucket, setQuotesByBucket] = useState<
    Record<string, BucketQuote>
  >({});

  const clientRef = useRef<QuotingClient | null>(null);
  // Track which request_id maps to which bucket_id.
  const pendingRef = useRef<Map<string, string>>(new Map());

  const handleResponse = useCallback(
    (requestId: string, payload: RfqResponsePayload) => {
      const bucketId =
        pendingRef.current.get(requestId) ?? payload.bucket_id;
      pendingRef.current.delete(requestId);
      setQuotesByBucket((prev) => ({
        ...prev,
        [bucketId]: {
          bucketId,
          quotes: payload.quotes,
          bestPremiumRaw:
            payload.quotes.length > 0
              ? payload.quotes[0].quote.premium
              : "0",
          updatedAt: Date.now(),
        },
      }));
    },
    [],
  );

  // WS lifecycle
  useEffect(() => {
    if (!enabled) return;
    const client = new QuotingClient(
      {
        onOpen: () => setConnected(true),
        onClose: () => setConnected(false),
        onRfqResponse: handleResponse,
        onError: (requestId) => {
          if (requestId) pendingRef.current.delete(requestId);
        },
      },
      url,
    );
    clientRef.current = client;
    client.connect();
    return () => {
      client.disconnect();
      clientRef.current = null;
    };
  }, [url, enabled, handleResponse]);

  // Send RFQs for every bucket in the series on a 1-minute interval.
  useEffect(() => {
    if (!enabled || !series || series.buckets.length === 0) return;

    const fire = () => {
      const client = clientRef.current;
      if (!client?.connected) return;
      for (const bucket of series.buckets) {
        const rid = nextRequestId();
        pendingRef.current.set(rid, bucket.bucket_id);
        client.sendRfq(
          {
            bucket_id: bucket.bucket_id,
            write_amount: writeAmount,
            side,
          },
          rid,
        );
      }
    };

    // Fire immediately, then every RFQ_INTERVAL_MS.
    fire();
    const interval = setInterval(fire, RFQ_INTERVAL_MS);
    return () => clearInterval(interval);
  }, [enabled, series, side, writeAmount]);

  return { connected, quotesByBucket };
}
