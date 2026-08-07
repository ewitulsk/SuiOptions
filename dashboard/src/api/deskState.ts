// mm-bot `GET /desk/state` (SO-348) — types mirror
// rust-backend/services/mm-bot/src/desk/state.rs DTOs (camelCase).

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

export type GreeksPerUnit = {
  delta: number;
  gamma: number;
  vega: number;
  theta: number;
};

export type Mark = {
  markPerUnit: number;
  value: number;
  sigma: number;
  spot: number;
  atMs: number;
  greeksPerUnit: GreeksPerUnit;
};

export type DeskHolding = {
  bucketId: string;
  optionCoinType: string;
  assetCoinType: string;
  symbol: string | null;
  isPut: boolean;
  strike: string;
  strikeScale: number;
  strikeScaled: number;
  expiryMs: number;
  amountVault: number;
  amountWallet: number;
  amountCoinPositions: number;
  amount: number;
  poolId: string | null;
  mark: Mark | null;
};

export type DeskWritten = {
  bucketId: string;
  positionId: string;
  assetCoinType: string;
  symbol: string | null;
  isPut: boolean;
  strike: string;
  strikeScale: number;
  strikeScaled: number;
  expiryMs: number;
  amount: number;
  covered: number;
  naked: number;
  mark: Mark | null;
};

export type GreeksAgg = {
  deltaUnits: number;
  gammaUnits: number;
  vega: number;
  thetaPerDay: number;
};

export type DeskLimits = {
  premium_budget_soft: number;
  premium_budget_hard: number;
  vega_cap_nav_per_volpt: number;
  theta_soft_nav_per_day: number;
  theta_hard_nav_per_day: number;
  per_expiry_max: number;
  per_strike_bucket_max: number;
  kill_drawdown: number;
  kill_window_days: number;
};

export type DeskVenue = {
  name: string;
  symbol: string;
  simulated: boolean;
  shortUnits: number;
  fundingRateAnnual: number;
  marginHeadroom: number;
  notional: number;
  realizedPnl: number;
  readOk: boolean;
};

export type SymbolHedge = {
  symbol: string;
  bookDeltaUnits: number;
  hedgeShortUnits: number;
  netUnits: number;
  bandUnits: number | null;
};

export type DeskMarket = {
  symbol: string;
  coinType: string;
  decimals: number;
  spot: number | null;
  spotAtMs: number | null;
  realizedVolShort: number | null;
  realizedVolLong: number | null;
  fallbackVol: number;
  surfaceIsFallback: boolean;
  carryYield: number;
};

export type DeskState = {
  enabled: true;
  generatedAtMs: number;
  bootedAtMs: number;
  network: string;
  vault: {
    vaultId: string;
    provisioned: boolean;
    curatorCap: string | null;
    curatorSessionFlowsEnabled: boolean;
    mmReleaseEnabled: boolean;
    settlementCoinType: string;
    settlementDecimals: number;
  };
  exposure: {
    nav: number;
    premiumDeployed: number;
    reserved: number;
    netVegaPerVolpt: number;
    thetaCostPerDay: number;
    premiumByExpiry: Record<string, number>;
    premiumByStrikeBucket: [number, number, number];
    killSwitch: boolean;
    stressBlocked: boolean;
  };
  utilization: { premium: number; vega: number; theta: number };
  limits: DeskLimits;
  greeks: {
    total: GreeksAgg;
    byExpiry: Array<{ expiryMs: number } & GreeksAgg>;
  };
  bookDeltaUnits: Record<string, number>;
  nakedWrittenUnits: number;
  fundingRateAnnual: number;
  stress: {
    atMs: number;
    gapDown60: number;
    gapUp80: number;
    flat6mo: number;
    fundingMinus50: number;
    worstDrawdown: number;
    blocked: boolean;
  } | null;
  holdings: DeskHolding[];
  written: DeskWritten[];
  reservations: {
    count: number;
    total: number;
    entries: Array<{ amount: number; expires_ms: number }>;
  };
  pnl: { spread: number; scalp: number; theta: number; funding: number; total: number };
  hedge: {
    bandPctNav: number;
    bandWidePctNav: number;
    fundingWidenThreshold: number;
    intervalSecs: number;
    venues: DeskVenue[];
    bySymbol: SymbolHedge[];
  };
  markets: DeskMarket[];
  config: {
    refreshSecs: number;
    expectedHoldingYears: number;
    surface: Record<string, number | null>;
    v1: Record<string, number>;
    v2: Record<string, number | boolean>;
    monitors: Record<string, number>;
    auctionsEnabled: boolean;
    exitsEnabled: boolean;
  };
};

export type DeskStateResponse = { enabled: false } | DeskState;

export async function fetchDeskState(mmBotBase: string): Promise<DeskStateResponse> {
  const res = await fetch(`${mmBotBase}/desk/state`);
  if (res.status === 503) throw new Error("desk starting (503)");
  if (!res.ok) throw new Error(`GET /desk/state failed: ${res.status}`);
  return (await res.json()) as DeskStateResponse;
}

export function useDeskState() {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["deskState", urls.mmBot],
    queryFn: () => fetchDeskState(urls.mmBot),
    refetchInterval: 15_000,
    retry: 1,
  });
}

/** Narrow a response to the enabled shape (undefined while loading/off). */
export function enabledState(r: DeskStateResponse | undefined): DeskState | undefined {
  return r && r.enabled ? r : undefined;
}
