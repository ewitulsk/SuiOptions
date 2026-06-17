// Typed client + react-query hook for `GET /options/metrics` (SO-198).
//
// Stateless Black-Scholes greeks, implied vol, fair value, and break-even for a
// single call. The browser holds live spot (Pyth) and the order-book mid
// (DeepBook devInspect) and passes them in; the endpoint is pure compute.
//
// Authoritative response shape: rust-backend/services/api-service/README.md
// (`GET /options/metrics`). UNITS: spot/strike/mark must share one unit.

import { keepPreviousData, useQuery } from "@tanstack/react-query";

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

export type OptionMetrics = {
  implied_vol: number | null;
  delta: number;
  gamma: number;
  vega: number; // per 1.00 vol — divide by 100 for per-1% display
  theta: number; // per calendar day
  rho: number; // per 1.00 rate
  break_even: number;
  fair_value: number | null;
};

export type MetricsParams = {
  spot: number;
  strike: number;
  t_years: number;
  mark: number; // order-book mid (quote) or avg cost (open position)
  r?: number;
};

export async function fetchOptionMetrics(p: MetricsParams): Promise<OptionMetrics> {
  const q = new URLSearchParams({
    spot: String(p.spot),
    strike: String(p.strike),
    t_years: String(p.t_years),
    mark: String(p.mark),
    ...(p.r != null ? { r: String(p.r) } : {}),
  });
  const res = await fetch(`${API_BASE_URL}/options/metrics?${q}`);
  if (!res.ok) throw new Error(`option metrics ${res.status}`);
  return res.json();
}

/** Years to expiry from an epoch-ms expiry. Floored at 0. */
export function yearsToExpiry(expiryMs: number, nowMs = Date.now()): number {
  return Math.max(0, (expiryMs - nowMs) / (365 * 24 * 60 * 60 * 1000));
}

/**
 * Live greeks/IV for a bucket. Re-fetches when spot or mid move (they're in the
 * queryKey) and on a ~1.5s poll so theta/τ stay fresh. Disabled until we have
 * spot + a positive mid.
 */
export function useOptionMetrics(args: {
  strike: number | null;
  expiryMs: number | null;
  spot: number | null;
  mid: number | null;
  r?: number;
  // Chain-table rows compute greeks for every strike; let them poll gently
  // (the selected strike keeps the default 1.5s cadence) — SO-225.
  refetchInterval?: number;
}) {
  const { strike, expiryMs, spot, mid, r, refetchInterval = 1500 } = args;
  const enabled =
    strike != null && expiryMs != null && spot != null && spot > 0 && mid != null && mid > 0;
  return useQuery({
    queryKey: ["option-metrics", strike, expiryMs, spot, mid, r ?? null],
    enabled,
    // Spot/mid live in the key, so every Pyth tick is a fresh query. Keep the
    // last result on screen while it refetches so greeks don't flash to "—"
    // between ticks (SO-225).
    placeholderData: keepPreviousData,
    refetchInterval,
    queryFn: () =>
      fetchOptionMetrics({
        spot: spot!,
        strike: strike!,
        t_years: yearsToExpiry(expiryMs!),
        mark: mid!,
        r,
      }),
  });
}
