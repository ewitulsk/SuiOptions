// Analytics endpoints (SO-390) on the Rust `api-service` backend.
//
// Same base-URL convention as `client.ts`: defaults to local dev, override
// with `VITE_API_BASE_URL`. Two endpoints:
//   GET /analytics/catalog — instruments + the metric params each supports
//   GET /analytics/series  — one (instrument, metric, params, range) series
//
// Errors: 400 `{"error":"..."}` for bad params; 503 `{"error":"analytics
// data unavailable"}` when the data lake is unreachable — surfaced as
// `AnalyticsUnavailableError` so the UI can show a friendly "temporarily
// unavailable" state instead of a generic failure.

import { useQuery } from "@tanstack/react-query";

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

export type AnalyticsInstrument = {
  /** e.g. `"btc-usdc.binance"`. */
  instrument_id: string;
  exchange: string;
  /** Display symbol, e.g. `"BTC-USDC"`. */
  symbol: string;
  metrics: {
    spot?: { freqs: string[] };
    rv?: {
      windows_s: number[];
      sample_intervals_s: number[];
      estimators: string[];
    };
  };
  /** ISO dates (`YYYY-MM-DD`) bounding the available history. */
  first_date: string;
  last_date: string;
};

export type AnalyticsCatalog = { instruments: AnalyticsInstrument[] };

export type AnalyticsSeries = {
  instrument_id: string;
  metric: "spot" | "rv";
  params: Record<string, unknown>;
  /** `[unix_ms, value]` pairs, ascending. RV values are annualized vol
   * fractions (multiply by 100 for percent); spot is in quote units. */
  points: [number, number][];
  meta: { units: string; points_dropped: number };
};

/** 503 from /analytics/* — the lake behind api-service is unreachable. */
export class AnalyticsUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AnalyticsUnavailableError";
  }
}

async function fetchAnalytics<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE_URL}${path}`);
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as { error?: string };
      if (body?.error) msg = body.error;
    } catch {
      // non-JSON error body; keep the status text
    }
    if (res.status === 503) throw new AnalyticsUnavailableError(msg);
    throw new Error(`GET ${path.split("?")[0]} failed: ${msg}`);
  }
  return (await res.json()) as T;
}

export type SpotSeriesParams = {
  instrument_id: string;
  freq: string;
  /** ISO date `YYYY-MM-DD`. */
  from: string;
  to: string;
};

export type RvSeriesParams = {
  instrument_id: string;
  window_s: number;
  sample_interval_s: number;
  estimator: string;
  from: string;
  to: string;
};

export function useAnalyticsCatalog() {
  return useQuery<AnalyticsCatalog, Error>({
    queryKey: ["analytics-catalog"],
    queryFn: () => fetchAnalytics<AnalyticsCatalog>("/analytics/catalog"),
    retry: 1,
  });
}

export function useSpotSeries(p: SpotSeriesParams | null) {
  return useQuery<AnalyticsSeries, Error>({
    queryKey: ["analytics-series", "spot", p],
    queryFn: () => {
      const qs = new URLSearchParams({
        instrument_id: p!.instrument_id,
        metric: "spot",
        freq: p!.freq,
        from: p!.from,
        to: p!.to,
      });
      return fetchAnalytics<AnalyticsSeries>(`/analytics/series?${qs}`);
    },
    enabled: p !== null,
    retry: 1,
  });
}

export function useRvSeries(p: RvSeriesParams | null) {
  return useQuery<AnalyticsSeries, Error>({
    queryKey: ["analytics-series", "rv", p],
    queryFn: () => {
      const qs = new URLSearchParams({
        instrument_id: p!.instrument_id,
        metric: "rv",
        window_s: String(p!.window_s),
        sample_interval_s: String(p!.sample_interval_s),
        estimator: p!.estimator,
        from: p!.from,
        to: p!.to,
      });
      return fetchAnalytics<AnalyticsSeries>(`/analytics/series?${qs}`);
    },
    enabled: p !== null,
    retry: 1,
  });
}
