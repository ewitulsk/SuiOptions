// mm-bot `GET /desk/history` (SO-349) — TimescaleDB-backed time series.

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

export type HistorySeries = "snapshots" | "symbols" | "venues" | "expiries" | "pnl";

export type SnapshotPoint = {
  timeMs: number;
  nav: number;
  deployed: number;
  reserved: number;
  netVegaPerVolpt: number;
  thetaCostPerDay: number;
  premiumUtil: number;
  vegaUtil: number;
  thetaUtil: number;
  premiumByStrikeBucket: [number, number, number];
  nakedUnits: number;
  fundingRateAnnual: number;
  killSwitch: boolean;
  stressBlocked: boolean;
  worstStressDrawdown: number | null;
};

export type SymbolPoint = {
  timeMs: number;
  symbol: string;
  spot: number | null;
  bookDeltaUnits: number;
  // Signed hedge position (positive = long — SO-428).
  hedgeUnits: number;
  netDeltaUnits: number;
  bandUnits: number | null;
};

export type VenuePoint = {
  timeMs: number;
  venue: string;
  symbol: string;
  // Signed perp position (positive = long — SO-428).
  positionUnits: number;
  fundingRateAnnual: number;
  marginHeadroom: number;
  notional: number;
  realizedPnl: number;
};

export type PnlPoint = { timeMs: number; line: string; amount: number; note: string };

export type HistoryResponse<P> = {
  series: HistorySeries;
  fromMs: number;
  toMs: number;
  bucketSecs: number;
  points: P[];
};

export async function fetchHistory<P>(
  mmBot: string,
  series: HistorySeries,
  params: { fromMs?: number; toMs?: number; bucketSecs?: number; symbol?: string } = {},
): Promise<HistoryResponse<P>> {
  const q = new URLSearchParams({ series });
  if (params.fromMs != null) q.set("fromMs", String(params.fromMs));
  if (params.toMs != null) q.set("toMs", String(params.toMs));
  if (params.bucketSecs != null) q.set("bucketSecs", String(params.bucketSecs));
  if (params.symbol) q.set("symbol", params.symbol);
  const res = await fetch(`${mmBot}/desk/history?${q}`);
  if (res.status === 404) throw new Error("history not configured on this environment");
  if (!res.ok) throw new Error(`GET /desk/history failed: ${res.status}`);
  return (await res.json()) as HistoryResponse<P>;
}

export function useDeskHistory<P>(
  series: HistorySeries,
  rangeHours: number,
  opts: { symbol?: string; enabled?: boolean } = {},
) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["deskHistory", urls.mmBot, series, rangeHours, opts.symbol],
    queryFn: () =>
      fetchHistory<P>(urls.mmBot, series, {
        fromMs: Date.now() - rangeHours * 3_600_000,
        symbol: opts.symbol,
      }),
    enabled: opts.enabled !== false,
    refetchInterval: 60_000,
    retry: 1,
  });
}
