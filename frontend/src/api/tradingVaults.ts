// HTTP client for the api-service curated trading-vault endpoints (SO-288).
//
// The list/detail endpoints ship snake_case DTOs (pps-history and stake ship
// camelCase); the wire types + mappers below convert to the app-facing
// camelCase types, and are the only place to adjust if the API shifts.
//
// Raw u128 fields (`totalSharesRaw`, `latestPpsE12Raw`) ship as decimal
// strings to preserve precision; use them when building a tx and the scaled
// helpers below for display.

import { normalizeStructTag } from "@mysten/sui/utils";

import { BLUEFIN_TEST_ENABLED, BLUEFIN_TEST_USDC, isBluefinTestUsdc } from "../bluefinTest";
import { SUPPORTED_TOKENS, type SupportedToken } from "../config";

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

/** Price-per-share fixed-point scale (`latestPpsE12Raw` is pps × 1e12). */
export const PPS_E12 = 1e12;

/** One curated trading vault. Mirrors the api-service trading-vault DTO. */
export type TradingVault = {
  vaultId: string;
  /** Canonical `0x…` coin type of the vault's single deposit asset. */
  depositAsset: string;
  creator: string;
  curator: string;
  curatorCapId: string;
  state: "open" | "closing" | "closed";
  lockupMs: number;
  curatorFeeBps: number;
  /** 0 = creator, 1 = curator, 2 = either. */
  rotationAuthority: number;
  maxPositions: number;
  unwindGraceMs: number;
  depositsPaused: boolean;
  mmReleaseEnabled: boolean;
  /** u128 decimal string, atomic share units. */
  totalSharesRaw: string;
  positionCount: number;
  pendingWithdrawals: number;
  /** u128 decimal string, pps × 1e12; null before the first appraisal. */
  latestPpsE12Raw: string | null;
  updatedAtMs: number;
  /** External MM account wallet (SO-299); null when none is set. */
  externalAccount: string | null;
  /** u64 decimal string, deposit-asset smallest units. */
  externalExposure: string;
  /** u64 decimal string, deposit-asset smallest units; null before the first
   * keeper-posted equity mark. */
  latestExternalEquity: string | null;
  /** Ms since epoch, or null before the first equity mark. Ships as a decimal
   * string on the wire; normalized to a number in the mapper. */
  externalEquityUpdatedAtMs: number | null;
  /** u128 decimal string, deposit-asset smallest units: NAV from the latest
   * consumed appraisal (SO-304); null before the first appraisal. */
  latestNavRaw: string | null;
  /** Ms since epoch, or null before the first appraisal. */
  navUpdatedAtMs: number | null;
};

/** One custodied position row from the detail endpoint. */
export type TradingVaultPosition = {
  positionId: string;
  adapter: string;
  active: boolean;
  storedAtMs: number;
  removedAtMs: number | null;
  /** u64 decimal string, deposit-asset smallest units: the latest appraisal
   * mark (SO-304); null until the position is first appraised. */
  lastValueRaw: string | null;
  /** Ms since epoch, or null until the first appraisal. */
  lastAppraisedAtMs: number | null;
};

export type TradingVaultDetail = TradingVault & {
  positions: TradingVaultPosition[];
};

/** Wire shape of one vault row: api-service ships these two endpoints in
 * snake_case (unlike pps-history/stake, which are camelCase). */
type TradingVaultWire = {
  vault_id: string;
  deposit_coin_type: string;
  creator: string;
  curator: string;
  curator_cap_id: string;
  state: "open" | "closing" | "closed";
  lockup_ms: number;
  curator_fee_bps: number;
  rotation_authority: number;
  max_positions: number;
  unwind_grace_ms: number;
  deposits_paused: boolean;
  mm_release_enabled: boolean;
  total_shares_raw: string;
  position_count: number;
  pending_withdrawals: number;
  /** pps × 1e12 decimal string; absent before the first appraisal. */
  pps_raw: string | null;
  updated_at_ms: number;
  external_account: string | null;
  /** u64 decimal string, deposit-asset smallest units. */
  external_exposure: string;
  latest_external_equity: string | null;
  /** Ms since epoch, decimal string. */
  external_equity_updated_at_ms: string | null;
  /** u128 decimal string; absent before the first consumed appraisal. */
  latest_nav_raw: string | null;
  nav_updated_at_ms: number | null;
};

type TradingVaultPositionWire = {
  position_id: string;
  adapter: string;
  active: boolean;
  stored_at_ms: number;
  removed_at_ms: number | null;
  /** u64 decimal string; absent until the position is first appraised. */
  last_value_raw: string | null;
  last_appraised_at_ms: number | null;
};

function mapVault(w: TradingVaultWire): TradingVault {
  return {
    vaultId: w.vault_id,
    depositAsset: w.deposit_coin_type,
    creator: w.creator,
    curator: w.curator,
    curatorCapId: w.curator_cap_id,
    state: w.state,
    lockupMs: w.lockup_ms,
    curatorFeeBps: w.curator_fee_bps,
    rotationAuthority: w.rotation_authority,
    maxPositions: w.max_positions,
    unwindGraceMs: w.unwind_grace_ms,
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

export async function fetchTradingVaults(): Promise<TradingVault[]> {
  const res = await fetch(`${API_BASE_URL}/trading-vaults`);
  if (!res.ok) {
    throw new Error(`GET /trading-vaults failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { vaults: TradingVaultWire[] };
  return body.vaults.map(mapVault);
}

export async function fetchTradingVault(vaultId: string): Promise<TradingVaultDetail> {
  const res = await fetch(`${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}`);
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id failed: ${res.status} ${res.statusText}`);
  }
  // Detail flattens the vault fields to the top level, plus positions.
  const body = (await res.json()) as TradingVaultWire & {
    positions: TradingVaultPositionWire[];
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
  };
}

/** One share-price sample from the pps-history endpoint (SO-293). */
export type TradingVaultPpsPoint = {
  /** Event time (ms since epoch), decimal string. */
  timestampMs: string;
  /** u128 decimal string, pps × 1e12. */
  ppsE12: string;
  /** `deposit` | `fulfillment`. */
  source: string;
};

export async function fetchTradingVaultPpsHistory(
  vaultId: string,
): Promise<TradingVaultPpsPoint[]> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/pps-history`,
  );
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id/pps-history failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { points: TradingVaultPpsPoint[] };
  return body.points;
}

/** The connected wallet's stake in one vault (SO-293). Raw integer fields
 * ship as decimal strings to preserve u128/u64 precision. */
export type TradingVaultStake = {
  /** u128 decimal string, atomic share units. */
  shares: string;
  /** u64 decimal string, deposit-asset smallest units. */
  costBasis: string;
  /** u64 decimal string at the latest pps, or null pre-appraisal. */
  estimatedValue: string | null;
  /** Ms since epoch, or null if the wallet never deposited. Ships as a
   * decimal string on the wire; normalized to a number in the fetcher. */
  lockedUntilMs: number | null;
};

export async function fetchTradingVaultStake(
  vaultId: string,
  address: string,
): Promise<TradingVaultStake> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/stake/${encodeURIComponent(address)}`,
  );
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id/stake/:address failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as Omit<TradingVaultStake, "lockedUntilMs"> & {
    lockedUntilMs: string | number | null;
  };
  return {
    ...body,
    // Serialized as a decimal string like the other raw-int fields.
    lockedUntilMs: body.lockedUntilMs == null ? null : Number(body.lockedUntilMs),
  };
}

/**
 * The supported-token catalog entry for a coin type, or null when the asset
 * isn't in the catalog. Coin types arrive in non-byte-equal forms (with and
 * without `0x`, padded and unpadded), so compare canonicalized on both sides.
 *
 * Off mainnet/prod, the Bluefin staging USDC (SO-311) resolves too: it is not
 * in token-info's catalog, so without this a vault holding it renders with a
 * hex symbol and unknown decimals — which disables deposits outright. On
 * mainnet/prod the fallback is compiled out and such a vault degrades to the
 * short type + "—" values, as any unknown asset does.
 */
export function tokenForCoinType(coinType: string | null | undefined): SupportedToken | null {
  if (!coinType) return null;
  let canonical: string;
  try {
    canonical = normalizeStructTag(coinType);
  } catch {
    return null;
  }
  return (
    SUPPORTED_TOKENS.find((t) => {
      try {
        return normalizeStructTag(t.coinType) === canonical;
      } catch {
        return false;
      }
    }) ??
    (BLUEFIN_TEST_ENABLED && isBluefinTestUsdc(canonical) ? BLUEFIN_TEST_USDC : null)
  );
}

/** Share price in deposit-asset units per share, or null pre-appraisal. */
export function tradingVaultPps(v: TradingVault): number | null {
  if (v.latestPpsE12Raw == null) return null;
  return Number(v.latestPpsE12Raw) / PPS_E12;
}

/**
 * TVL estimate in display units of the deposit asset:
 * totalShares × pps / 10^decimals. Null when pps or decimals are unknown.
 */
export function tradingVaultTvl(v: TradingVault, decimals: number | null): number | null {
  const pps = tradingVaultPps(v);
  if (pps == null || decimals == null) return null;
  return (Number(v.totalSharesRaw) * pps) / 10 ** decimals;
}
