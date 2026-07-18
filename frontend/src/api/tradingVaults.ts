// HTTP client for the api-service curated trading-vault endpoints (SO-288).
//
// The backend handlers are being built in parallel; these types mirror the
// agreed DTO shapes (camelCase, unlike the older snake_case vault DTOs). If
// field names shift at integration, this file is the only place to adjust.
//
// Raw u128 fields (`totalSharesRaw`, `latestPpsE12Raw`) ship as decimal
// strings to preserve precision; use them when building a tx and the scaled
// helpers below for display.

import { normalizeStructTag } from "@mysten/sui/utils";

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
};

/** One custodied position row from the detail endpoint. */
export type TradingVaultPosition = {
  positionId: string;
  adapter: string;
  active: boolean;
  storedAtMs: number;
  removedAtMs: number | null;
};

export type TradingVaultDetail = TradingVault & {
  positions: TradingVaultPosition[];
};

export type TradingVaultsResponse = { vaults: TradingVault[] };

export async function fetchTradingVaults(): Promise<TradingVault[]> {
  const res = await fetch(`${API_BASE_URL}/trading-vaults`);
  if (!res.ok) {
    throw new Error(`GET /trading-vaults failed: ${res.status} ${res.statusText}`);
  }
  const body: TradingVaultsResponse = await res.json();
  return body.vaults;
}

export async function fetchTradingVault(vaultId: string): Promise<TradingVaultDetail> {
  const res = await fetch(`${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}`);
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as TradingVaultDetail;
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
    }) ?? null
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
