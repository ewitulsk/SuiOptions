// api-service trading-vault reads — trimmed adaptation of
// frontend/src/api/tradingVaults.ts (list/detail are snake_case on the
// wire; pps-history is camelCase).

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

export const PPS_E12 = 1e12;

export type TradingVault = {
  vaultId: string;
  depositAsset: string;
  creator: string;
  curator: string;
  curatorCapId: string;
  state: "open" | "closing" | "closed";
  lockupMs: number;
  curatorFeeBps: number;
  depositsPaused: boolean;
  mmReleaseEnabled: boolean;
  totalSharesRaw: string;
  positionCount: number;
  pendingWithdrawals: number;
  latestPpsE12Raw: string | null;
  updatedAtMs: number;
  externalAccount: string | null;
  externalExposure: string;
  latestExternalEquity: string | null;
  externalEquityUpdatedAtMs: number | null;
  latestNavRaw: string | null;
  navUpdatedAtMs: number | null;
};

export type TradingVaultPosition = {
  positionId: string;
  adapter: string;
  active: boolean;
  storedAtMs: number;
  removedAtMs: number | null;
  lastValueRaw: string | null;
  lastAppraisedAtMs: number | null;
};

export type TradingVaultBalance = {
  coinType: string;
  symbol: string;
  decimals: number | null;
  amountRaw: string;
};

export type TradingVaultDetail = TradingVault & {
  positions: TradingVaultPosition[];
  balances: TradingVaultBalance[];
  balancesStale: boolean;
};

type Wire = {
  vault_id: string;
  deposit_coin_type: string;
  creator: string;
  curator: string;
  curator_cap_id: string;
  state: "open" | "closing" | "closed";
  lockup_ms: number;
  curator_fee_bps: number;
  deposits_paused: boolean;
  mm_release_enabled: boolean;
  total_shares_raw: string;
  position_count: number;
  pending_withdrawals: number;
  pps_raw: string | null;
  updated_at_ms: number;
  external_account: string | null;
  external_exposure: string;
  latest_external_equity: string | null;
  external_equity_updated_at_ms: string | null;
  latest_nav_raw: string | null;
  nav_updated_at_ms: number | null;
};

function mapVault(w: Wire): TradingVault {
  return {
    vaultId: w.vault_id,
    depositAsset: w.deposit_coin_type,
    creator: w.creator,
    curator: w.curator,
    curatorCapId: w.curator_cap_id,
    state: w.state,
    lockupMs: w.lockup_ms,
    curatorFeeBps: w.curator_fee_bps,
    depositsPaused: w.deposits_paused,
    mmReleaseEnabled: w.mm_release_enabled,
    totalSharesRaw: w.total_shares_raw,
    positionCount: w.position_count,
    pendingWithdrawals: w.pending_withdrawals,
    latestPpsE12Raw: w.pps_raw ?? null,
    updatedAtMs: w.updated_at_ms,
    externalAccount: w.external_account ?? null,
    externalExposure: w.external_exposure,
    latestExternalEquity: w.latest_external_equity ?? null,
    externalEquityUpdatedAtMs:
      w.external_equity_updated_at_ms == null ? null : Number(w.external_equity_updated_at_ms),
    latestNavRaw: w.latest_nav_raw ?? null,
    navUpdatedAtMs: w.nav_updated_at_ms ?? null,
  };
}

async function fetchVaultDetail(api: string, vaultId: string): Promise<TradingVaultDetail> {
  const res = await fetch(`${api}/trading-vaults/${encodeURIComponent(vaultId)}`);
  if (!res.ok) throw new Error(`GET /trading-vaults/:id failed: ${res.status}`);
  const body = (await res.json()) as Wire & {
    positions: Array<{
      position_id: string;
      adapter: string;
      active: boolean;
      stored_at_ms: number;
      removed_at_ms: number | null;
      last_value_raw: string | null;
      last_appraised_at_ms: number | null;
    }>;
    balances?: Array<{
      coin_type: string;
      symbol: string;
      decimals: number | null;
      amount_raw: string;
    }>;
    balances_stale?: boolean;
  };
  return {
    ...mapVault(body),
    positions: body.positions.map((p) => ({
      positionId: p.position_id,
      adapter: p.adapter,
      active: p.active,
      storedAtMs: p.stored_at_ms,
      removedAtMs: p.removed_at_ms ?? null,
      lastValueRaw: p.last_value_raw ?? null,
      lastAppraisedAtMs: p.last_appraised_at_ms ?? null,
    })),
    balances: (body.balances ?? []).map((b) => ({
      coinType: b.coin_type,
      symbol: b.symbol,
      decimals: b.decimals ?? null,
      amountRaw: b.amount_raw,
    })),
    balancesStale: body.balances == null || body.balances_stale === true,
  };
}

export function useVaultDetail(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["vaultDetail", urls.api, vaultId],
    queryFn: () => fetchVaultDetail(urls.api, vaultId as string),
    enabled: Boolean(vaultId),
    refetchInterval: 30_000,
  });
}

export type PpsPoint = { timestampMs: string; ppsE12: string; source: string };

export function usePpsHistory(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["ppsHistory", urls.api, vaultId],
    queryFn: async (): Promise<PpsPoint[]> => {
      const res = await fetch(
        `${urls.api}/trading-vaults/${encodeURIComponent(vaultId as string)}/pps-history`,
      );
      if (!res.ok) throw new Error(`GET pps-history failed: ${res.status}`);
      const body = (await res.json()) as { points: PpsPoint[] };
      return body.points;
    },
    enabled: Boolean(vaultId),
    refetchInterval: 60_000,
  });
}

/** TVL in deposit-asset raw units: totalShares × pps / 1e12. */
export function vaultTvlRaw(v: TradingVault): number | null {
  if (v.latestPpsE12Raw == null) return null;
  return (Number(v.totalSharesRaw) * Number(v.latestPpsE12Raw)) / PPS_E12;
}
