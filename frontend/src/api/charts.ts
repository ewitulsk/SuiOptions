// price-charting client (SO-157): REST history + WS live bar updates.
//
// `useBars(poolId, interval)` fetches the last ~300 bars, then keeps the tail
// fresh over the WS — the server re-sends the CURRENT bar on every fill (and
// on subscribe), so merging is a simple upsert-by-timestamp. On reconnect the
// REST tail is re-fetched to heal anything missed while disconnected.

import { useEffect, useRef, useState } from "react";

import { CHARTS_URL } from "../config";

export type ChartBar = {
  /** Bucket start, unix ms. */
  t: number;
  o: number;
  h: number;
  l: number;
  c: number;
  /** Base volume in display units. */
  v: number;
};

export type ChartInterval = "1m" | "5m" | "15m" | "1h" | "4h" | "1d";

export const CHART_INTERVALS: ChartInterval[] = ["1m", "5m", "15m", "1h", "4h", "1d"];

const INTERVAL_MS: Record<ChartInterval, number> = {
  "1m": 60_000,
  "5m": 300_000,
  "15m": 900_000,
  "1h": 3_600_000,
  "4h": 14_400_000,
  "1d": 86_400_000,
};

const HISTORY_BARS = 300;

export async function fetchBars(
  poolId: string,
  interval: ChartInterval,
  signal?: AbortSignal,
): Promise<ChartBar[]> {
  const base = CHARTS_URL.replace(/\/$/, "");
  const to = Date.now() + INTERVAL_MS[interval];
  const from = to - HISTORY_BARS * INTERVAL_MS[interval];
  const url = `${base}/bars?pool_id=${encodeURIComponent(poolId)}&interval=${interval}&from_ms=${from}&to_ms=${to}`;
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`charts /bars → ${res.status}`);
  const body = (await res.json()) as { bars: ChartBar[] };
  return body.bars;
}

function wsUrl(): string {
  const base = CHARTS_URL.replace(/\/$/, "").replace(/^http/, "ws");
  return `${base}/ws`;
}

/**
 * Live bar series for one (pool, interval). `bars` is sorted ascending by
 * `t`; the last element mutates in place as fills land.
 */
export function useBars(poolId: string | null, interval: ChartInterval) {
  const [bars, setBars] = useState<ChartBar[]>([]);
  const [loading, setLoading] = useState(true);
  // Bump to force a reconnect-and-refetch cycle after a WS drop.
  const [epoch, setEpoch] = useState(0);
  const retryRef = useRef(0);

  useEffect(() => {
    if (!poolId) {
      setBars([]);
      setLoading(false);
      return;
    }
    let alive = true;
    const abort = new AbortController();
    setLoading(true);

    fetchBars(poolId, interval, abort.signal)
      .then((b) => {
        if (alive) {
          setBars(b);
          setLoading(false);
          retryRef.current = 0;
        }
      })
      .catch((e) => {
        if (alive && e?.name !== "AbortError") {
          console.warn("charts history fetch failed:", e);
          setLoading(false);
        }
      });

    let ws: WebSocket | null = null;
    try {
      ws = new WebSocket(wsUrl());
      ws.onopen = () => {
        ws?.send(JSON.stringify({ type: "subscribe", pool_id: poolId, interval }));
      };
      ws.onmessage = (ev) => {
        try {
          const msg = JSON.parse(ev.data as string);
          if (msg.type === "bar" && msg.pool_id === poolId && msg.interval === interval) {
            const bar = msg.bar as ChartBar;
            setBars((prev) => {
              const i = prev.findIndex((b) => b.t === bar.t);
              if (i >= 0) {
                const next = prev.slice();
                next[i] = bar;
                return next;
              }
              return [...prev, bar].sort((a, b) => a.t - b.t);
            });
          }
        } catch {
          // non-JSON frame (ping); ignore
        }
      };
      ws.onclose = () => {
        if (!alive) return;
        // Reconnect with capped backoff; the effect rerun re-fetches history
        // so the gap heals.
        const delay = Math.min(15_000, 1_000 * 2 ** retryRef.current++);
        setTimeout(() => {
          if (alive) setEpoch((e) => e + 1);
        }, delay);
      };
    } catch (e) {
      console.warn("charts ws connect failed:", e);
    }

    return () => {
      alive = false;
      abort.abort();
      ws?.close();
    };
  }, [poolId, interval, epoch]);

  return { bars, loading };
}
