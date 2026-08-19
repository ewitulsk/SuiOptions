// HTTP client for the api-service curated trading-vault endpoints (SO-288,
// v2 overhaul SO-418).
//
// The endpoints ship snake_case DTOs; the wire types + mappers below convert
// to the app-facing camelCase types, and are the only place to adjust if the
// API shifts.
//
// Raw u128/u64 fields (`totalSharesRaw`, `latestPpsE12Raw`, …) ship as
// decimal strings to preserve precision; use them when building a tx and the
// scaled helpers below for display.

import { normalizeStructTag } from "@mysten/sui/utils";

import { BLUEFIN_TEST_ENABLED, BLUEFIN_TEST_USDC, isBluefinTestUsdc } from "../bluefinTest";
import { SUPPORTED_TOKENS, type SupportedToken } from "../config";

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

/** Price-per-share fixed-point scale (`latestPpsE12Raw` is pps × 1e12). */
export const PPS_E12 = 1e12;

/**
 * Virtual-offset share scale (SO-370): genesis mints `value × 1e6` shares
 * against one virtual asset unit, so raw shares are 1e6× the accounting
 * asset's raw units at pps 1. Display shares and pps rescale by this. v2
 * keeps the same offset per tranche.
 */
export const SHARE_OFFSET = 1e6;

// ═════════════════════════ v2 capital-structure types ═════════════════════════

/** Wire tranche label (codes: 0 untranched / 1 senior / 2 junior). */
export type TrancheLabel = "untranched" | "senior" | "junior";

/** Wire risk-state label (codes 0..3). */
export type RiskStateLabel = "healthy" | "coverage_breach" | "impaired" | "reset_pending";

/** Wire withdrawal-lane label (codes: 0 senior / 1 junior; untranched vaults
 * use the junior lane). */
export type LaneLabel = "senior" | "junior";

export type UpsideMode = "preferred_only" | "capped_participating" | "uncapped_participating";

/** Immutable senior/junior terms; null on untranched vaults. */
export type VaultCapitalStructure = {
  seniorHurdleBpsAnnual: number;
  targetJuniorBps: number;
  maintenanceJuniorBps: number;
  upside: UpsideMode;
  residualParticipationBps: number;
  totalReturnCapBps: number;
};

/** A pending junior generational reset (§8.5); null when none. */
export type VaultResetProposal = {
  oldGeneration: number;
  proposedAtMs: number;
  executableAtMs: number;
  /** u128 decimal strings, accounting-asset smallest units. */
  recordedNavRaw: string;
  recordedSeniorClaimRaw: string;
  recordedRequiredDepositRaw: string;
};

export type LaneBounds = { head: number; tail: number };

/** One curated trading vault. Mirrors the api-service trading-vault DTO. */
export type TradingVault = {
  vaultId: string;
  /** Canonical `0x…` coin type of the vault's ACCOUNTING asset (SO-370:
   * `config.accounting_asset` — the unit of account; deposits/payouts may
   * be any asset on the vault's `deposit_assets` allowlist). */
  accountingAsset: string;
  creator: string;
  curator: string;
  curatorCapId: string;
  state: "open" | "closing" | "closed";
  lockupMs: number;
  curatorFeeBps: number;
  unwindGraceMs: number;
  depositsPaused: boolean;
  mmReleaseEnabled: boolean;
  /** u128 decimal string, atomic share units (senior + junior). */
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

  // ── v2 capital structure & risk state (SO-418) ──
  /** Immutable at creation; null for untranched vaults. */
  capitalStructure: VaultCapitalStructure | null;
  termsVersion: number;
  /** Hex spec hash of the governing terms, or null when absent. */
  specHash: string | null;
  riskState: RiskStateLabel;
  riskStateCode: number;
  curatorCommitmentBreached: boolean;
  /** u128 decimal strings, atomic share units. */
  seniorSharesRaw: string;
  juniorSharesRaw: string;
  /** u128 decimal string, accounting-asset smallest units. */
  seniorClaimRaw: string;
  /** From the latest TvCapitalSynced; null before the first sync. */
  seniorNavRaw: string | null;
  juniorNavRaw: string | null;
  /** Per-tranche observed pps (1e12-scaled float + raw string). */
  seniorPps: number | null;
  seniorPpsRaw: string | null;
  juniorPps: number | null;
  juniorPpsRaw: string | null;
  /** junior_nav × 1e4 / nav from the latest sync; null before it. */
  juniorBufferBps: number | null;
  impairedSinceMs: number | null;
  activeJuniorGeneration: number;
  resetProposal: VaultResetProposal | null;
  /** True once the terminal settlement snapshot has run (§8.7). */
  settled: boolean;
  laneHeads: { senior: LaneBounds; junior: LaneBounds } | null;
};

/** One CUSTODIED position row from the detail endpoint — an adapter-held
 * object the appraisal walks (renamed from `TradingVaultPosition` so the
 * `VaultPosition` claim NFT below owns the plain name). */
export type VaultHoldingPosition = {
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

/**
 * One free balance the vault holds outside custody (SO-313) — a
 * `vault::BalanceKey<T>` dynamic field on the vault object. Includes the
 * deposit asset. Distinct from a *holding position*, which is a custodied
 * object the appraisal walks; a curator spot trade only ever moves free
 * balances.
 */
export type TradingVaultBalance = {
  /** Canonical `0x…::mod::T` coin type. */
  coinType: string;
  /** Backend symbol, falling back to the coin type when uncatalogued. */
  symbol: string;
  /** Null when the asset isn't in the catalog — render the raw amount then. */
  decimals: number | null;
  /** u64 decimal string, atomic units. */
  amountRaw: string;
};

export type TradingVaultDetail = TradingVault & {
  positions: VaultHoldingPosition[];
  balances: TradingVaultBalance[];
  /**
   * The live balance read failed, so `balances` is *unknown* rather than
   * empty. Never render an empty list as "holds nothing" while this is set.
   */
  balancesStale: boolean;
};

type CapitalStructureWire = {
  senior_hurdle_bps_annual: number;
  target_junior_bps: number;
  maintenance_junior_bps: number;
  upside: UpsideMode;
  residual_participation_bps: number;
  total_return_cap_bps: number;
};

type ResetProposalWire = {
  old_generation: number;
  proposed_at_ms: number;
  executable_at_ms: number;
  recorded_nav_raw: string;
  recorded_senior_claim_raw: string;
  recorded_required_deposit_raw: string;
};

/** Wire shape of one vault row (snake_case, per the WS-3 DTO contract). */
type TradingVaultWire = {
  vault_id: string;
  /** SO-370 rename; api-services predating it send `deposit_coin_type`. */
  accounting_coin_type?: string;
  deposit_coin_type?: string;
  creator: string;
  curator: string;
  curator_cap_id: string;
  state: "open" | "closing" | "closed";
  lockup_ms: number;
  curator_fee_bps: number;
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
  // ── v2 additions (marked optional so the UI degrades to untranched /
  // healthy defaults against an api-service predating SO-418) ──
  capital_structure?: CapitalStructureWire | null;
  terms_version?: number;
  spec_hash?: string | null;
  risk_state?: RiskStateLabel;
  risk_state_code?: number;
  curator_commitment_breached?: boolean;
  senior_shares_raw?: string;
  junior_shares_raw?: string;
  senior_claim_raw?: string;
  senior_nav_raw?: string | null;
  junior_nav_raw?: string | null;
  senior_pps?: number | null;
  senior_pps_raw?: string | null;
  junior_pps?: number | null;
  junior_pps_raw?: string | null;
  junior_buffer_bps?: number | null;
  impaired_since_ms?: number | null;
  active_junior_generation?: number;
  reset_proposal?: ResetProposalWire | null;
  settled?: boolean;
  lane_heads?: { senior: LaneBounds; junior: LaneBounds } | null;
};

type TradingVaultBalanceWire = {
  coin_type: string;
  symbol: string;
  decimals: number | null;
  amount_raw: string;
};

type VaultHoldingPositionWire = {
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
  const accountingAsset = w.accounting_coin_type ?? w.deposit_coin_type;
  if (!accountingAsset) {
    throw new Error(`vault ${w.vault_id} has no accounting coin type in the API response`);
  }
  return {
    vaultId: w.vault_id,
    accountingAsset,
    creator: w.creator,
    curator: w.curator,
    curatorCapId: w.curator_cap_id,
    state: w.state,
    lockupMs: w.lockup_ms,
    curatorFeeBps: w.curator_fee_bps,
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
    capitalStructure:
      w.capital_structure == null
        ? null
        : {
            seniorHurdleBpsAnnual: w.capital_structure.senior_hurdle_bps_annual,
            targetJuniorBps: w.capital_structure.target_junior_bps,
            maintenanceJuniorBps: w.capital_structure.maintenance_junior_bps,
            upside: w.capital_structure.upside,
            residualParticipationBps: w.capital_structure.residual_participation_bps,
            totalReturnCapBps: w.capital_structure.total_return_cap_bps,
          },
    termsVersion: w.terms_version ?? 1,
    specHash: w.spec_hash ?? null,
    riskState: w.risk_state ?? "healthy",
    riskStateCode: w.risk_state_code ?? 0,
    curatorCommitmentBreached: w.curator_commitment_breached ?? false,
    seniorSharesRaw: w.senior_shares_raw ?? "0",
    juniorSharesRaw: w.junior_shares_raw ?? w.total_shares_raw,
    seniorClaimRaw: w.senior_claim_raw ?? "0",
    seniorNavRaw: w.senior_nav_raw ?? null,
    juniorNavRaw: w.junior_nav_raw ?? null,
    seniorPps: w.senior_pps ?? null,
    seniorPpsRaw: w.senior_pps_raw ?? null,
    juniorPps: w.junior_pps ?? null,
    juniorPpsRaw: w.junior_pps_raw ?? null,
    juniorBufferBps: w.junior_buffer_bps ?? null,
    impairedSinceMs: w.impaired_since_ms ?? null,
    activeJuniorGeneration: w.active_junior_generation ?? 0,
    resetProposal:
      w.reset_proposal == null
        ? null
        : {
            oldGeneration: w.reset_proposal.old_generation,
            proposedAtMs: w.reset_proposal.proposed_at_ms,
            executableAtMs: w.reset_proposal.executable_at_ms,
            recordedNavRaw: w.reset_proposal.recorded_nav_raw,
            recordedSeniorClaimRaw: w.reset_proposal.recorded_senior_claim_raw,
            recordedRequiredDepositRaw: w.reset_proposal.recorded_required_deposit_raw,
          },
    settled: w.settled ?? false,
    laneHeads: w.lane_heads ?? null,
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
  // Detail flattens the vault fields to the top level, plus custodied
  // positions and free balances.
  const body = (await res.json()) as TradingVaultWire & {
    positions: VaultHoldingPositionWire[];
    // Optional so the app keeps working against an api-service that predates
    // SO-313 — an absent field reads as "unknown", not "holds nothing".
    balances?: TradingVaultBalanceWire[];
    balances_stale?: boolean;
  };
  return {
    ...mapVault(body),
    balances: (body.balances ?? []).map((b) => ({
      coinType: b.coin_type,
      symbol: b.symbol,
      decimals: b.decimals ?? null,
      amountRaw: b.amount_raw,
    })),
    balancesStale: body.balances == null || body.balances_stale === true,
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

// ═════════════════════════ pps history (per tranche) ═════════════════════════

/** One share-price sample. v2 (SO-418): per-tranche series with junior
 * generation-reset markers; untranched vaults emit `tranche: "untranched"`. */
export type TradingVaultPpsPoint = {
  timestampMs: number;
  tranche: TrancheLabel;
  /** Display share price (1e12-scaled float already divided down). */
  pps: number;
  /** u128 decimal string, pps × 1e12. */
  ppsRaw: string;
  /** `deposit` | `withdraw` | `capital_sync`. */
  source: string;
  /** True on the first junior point of a new generation (pps re-bases). */
  reset: boolean;
};

type TradingVaultPpsPointWire = {
  timestamp_ms: number;
  tranche: TrancheLabel;
  pps: number;
  pps_raw: string;
  source: string;
  reset: boolean;
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
  const body = (await res.json()) as { points: TradingVaultPpsPointWire[] };
  return body.points.map((p) => ({
    timestampMs: p.timestamp_ms,
    tranche: p.tranche,
    pps: p.pps,
    ppsRaw: p.pps_raw,
    source: p.source,
    reset: p.reset,
  }));
}

/**
 * One curator spot trade against an allowlisted DeepBook pool (SO-313).
 * The event states only the direction; resolve `poolId` against the pool
 * allowlist to name the two assets.
 */
export type TradingVaultTrade = {
  /** Ms since epoch. Ships as a decimal string; normalized in the fetcher. */
  timestampMs: number;
  txDigest: string;
  poolId: string;
  /** True when the vault sold the pool's base asset for its quote asset. */
  baseForQuote: boolean;
  /** u64 decimal strings, atomic units of the respective assets. */
  amountIn: string;
  amountOut: string;
  /** Input returned unfilled (lot rounding or a thin book). */
  unswapped: string;
};

/** The vault's curator spot trades, newest first (SO-313). */
export async function fetchTradingVaultTrades(vaultId: string): Promise<TradingVaultTrade[]> {
  const res = await fetch(`${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/trades`);
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id/trades failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as {
    trades: (Omit<TradingVaultTrade, "timestampMs"> & { timestampMs: string | number })[];
  };
  return body.trades.map((t) => ({ ...t, timestampMs: Number(t.timestampMs) }));
}

// ═════════════════════════ VaultPosition claim NFTs ═════════════════════════

/**
 * One wallet-held `vault_position::VaultPosition` claim NFT (SO-418).
 * Replaces the address-keyed stake: a wallet may hold N positions per
 * vault, each with its own shares, basis, lockup, tranche, and junior
 * generation. Raw integer fields ship as decimal strings.
 */
export type VaultPosition = {
  positionId: string;
  vaultId: string;
  tranche: TrancheLabel;
  trancheCode: number;
  capitalGeneration: number;
  /** True for a junior position of a wiped (pre-reset) generation —
   * permanently zero value. */
  wiped: boolean;
  /** u128 decimal string, atomic share units. */
  sharesRaw: string;
  /** u64 decimal string, accounting-asset smallest units. */
  costBasisRaw: string;
  lockedUntilMs: number;
  /** shares × (nav_t+1)/(S_t+OFFSET) at the latest tranche ratio; null
   * before the first capital sync. */
  estimatedValueRaw: string | null;
  /** max(value − basis, 0). */
  estimatedProfitRaw: string | null;
  /** profit × curator_fee_bps / 1e4 — the embedded fee liability. */
  estimatedFeeRaw: string | null;
};

type VaultPositionWire = {
  position_id: string;
  vault_id: string;
  tranche: TrancheLabel;
  tranche_code: number;
  capital_generation: number;
  wiped: boolean;
  shares_raw: string;
  cost_basis_raw: string;
  locked_until_ms: number;
  estimated_value_raw: string | null;
  estimated_profit_raw: string | null;
  estimated_fee_raw: string | null;
};

function mapPosition(w: VaultPositionWire): VaultPosition {
  return {
    positionId: w.position_id,
    vaultId: w.vault_id,
    tranche: w.tranche,
    trancheCode: w.tranche_code,
    capitalGeneration: w.capital_generation,
    wiped: w.wiped,
    sharesRaw: w.shares_raw,
    costBasisRaw: w.cost_basis_raw,
    lockedUntilMs: w.locked_until_ms,
    estimatedValueRaw: w.estimated_value_raw ?? null,
    estimatedProfitRaw: w.estimated_profit_raw ?? null,
    estimatedFeeRaw: w.estimated_fee_raw ?? null,
  };
}

/** `GET /trading-vaults/:id/positions/:address` — the wallet's live
 * VaultPosition NFTs in one vault (JIT owned-object query, SO-418). */
export async function fetchVaultPositions(
  vaultId: string,
  address: string,
): Promise<VaultPosition[]> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/positions/${encodeURIComponent(address)}`,
  );
  if (!res.ok) {
    throw new Error(
      `GET /trading-vaults/:id/positions/:address failed: ${res.status} ${res.statusText}`,
    );
  }
  const body = (await res.json()) as { positions: VaultPositionWire[] };
  return body.positions.map(mapPosition);
}

/** A position looked up by id — works for ANY holder (positions are freely
 * transferable; a prospective secondary buyer renders this pre-purchase). */
export type VaultPositionDetail = VaultPosition & {
  /** Live object owner, or null when not wallet-owned. */
  owner: string | null;
};

/** `GET /trading-vaults/positions/:positionId` — 404s when the object
 * doesn't exist or isn't a VaultPosition. */
export async function fetchVaultPositionDetail(positionId: string): Promise<VaultPositionDetail> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/positions/${encodeURIComponent(positionId)}`,
  );
  if (!res.ok) {
    throw new Error(
      res.status === 404
        ? `No VaultPosition exists at ${positionId}`
        : `GET /trading-vaults/positions/:id failed: ${res.status} ${res.statusText}`,
    );
  }
  const body = (await res.json()) as VaultPositionWire & { owner: string | null };
  return { ...mapPosition(body), owner: body.owner ?? null };
}

// ═════════════════════════════ waterfall ═════════════════════════════

/** The §3.4a waterfall decomposition at the latest capital sync (SO-418).
 * Powers the tranche stat strip and the client-side deposit-buffer check. */
export type VaultWaterfall = {
  /** u128 decimal strings, accounting-asset smallest units. */
  navRaw: string;
  seniorClaimRaw: string;
  seniorPrincipalBasisRaw: string | null;
  preferredRaw: string;
  participationRaw: string;
  seniorNavRaw: string;
  juniorNavRaw: string;
  juniorBufferBps: number;
  targetJuniorBps: number;
  maintenanceJuniorBps: number;
  upside: UpsideMode;
  residualParticipationBps: number;
  totalReturnCapBps: number;
  riskState: RiskStateLabel;
  riskStateCode: number;
  /** u128 decimal strings, atomic share units. */
  seniorSharesRaw: string;
  juniorSharesRaw: string;
  updatedAtMs: number;
};

type VaultWaterfallWire = {
  nav_raw: string;
  senior_claim_raw: string;
  senior_principal_basis_raw: string | null;
  preferred_raw: string;
  participation_raw: string;
  senior_nav_raw: string;
  junior_nav_raw: string;
  junior_buffer_bps: number;
  target_junior_bps: number;
  maintenance_junior_bps: number;
  upside: UpsideMode;
  residual_participation_bps: number;
  total_return_cap_bps: number;
  risk_state: RiskStateLabel;
  risk_state_code: number;
  senior_shares_raw: string;
  junior_shares_raw: string;
  updated_at_ms: number;
};

/** `GET /trading-vaults/:id/waterfall` (SO-418). */
export async function fetchVaultWaterfall(vaultId: string): Promise<VaultWaterfall> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/waterfall`,
  );
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id/waterfall failed: ${res.status} ${res.statusText}`);
  }
  const w = (await res.json()) as VaultWaterfallWire;
  return {
    navRaw: w.nav_raw,
    seniorClaimRaw: w.senior_claim_raw,
    seniorPrincipalBasisRaw: w.senior_principal_basis_raw ?? null,
    preferredRaw: w.preferred_raw,
    participationRaw: w.participation_raw,
    seniorNavRaw: w.senior_nav_raw,
    juniorNavRaw: w.junior_nav_raw,
    juniorBufferBps: w.junior_buffer_bps,
    targetJuniorBps: w.target_junior_bps,
    maintenanceJuniorBps: w.maintenance_junior_bps,
    upside: w.upside,
    residualParticipationBps: w.residual_participation_bps,
    totalReturnCapBps: w.total_return_cap_bps,
    riskState: w.risk_state,
    riskStateCode: w.risk_state_code,
    seniorSharesRaw: w.senior_shares_raw,
    juniorSharesRaw: w.junior_shares_raw,
    updatedAtMs: w.updated_at_ms,
  };
}

// ═════════════════════════════ settlement ═════════════════════════════

/** Terminal settlement pool state (§8.7): `{ settled: false }` before the
 * snapshot, frozen entitlements after. */
export type VaultSettlement =
  | { settled: false }
  | {
      settled: true;
      finalNavRaw: string;
      seniorPoolRaw: string;
      seniorSupplyRaw: string;
      juniorPoolRaw: string;
      juniorSupplyRaw: string;
      activeJuniorGeneration: number;
      /** Sum of TvSettlementRedeemed payouts vs the remainder. */
      redeemedRaw: string;
      outstandingRaw: string;
      snapshotAtMs: number;
    };

type VaultSettlementWire = {
  settled: boolean;
  final_nav_raw?: string;
  senior_pool_raw?: string;
  senior_supply_raw?: string;
  junior_pool_raw?: string;
  junior_supply_raw?: string;
  active_junior_generation?: number;
  redeemed_raw?: string;
  outstanding_raw?: string;
  snapshot_at_ms?: number;
};

/** `GET /trading-vaults/:id/settlement` (SO-418). */
export async function fetchVaultSettlement(vaultId: string): Promise<VaultSettlement> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/settlement`,
  );
  if (!res.ok) {
    throw new Error(`GET /trading-vaults/:id/settlement failed: ${res.status} ${res.statusText}`);
  }
  const w = (await res.json()) as VaultSettlementWire;
  if (!w.settled) return { settled: false };
  return {
    settled: true,
    finalNavRaw: w.final_nav_raw ?? "0",
    seniorPoolRaw: w.senior_pool_raw ?? "0",
    seniorSupplyRaw: w.senior_supply_raw ?? "0",
    juniorPoolRaw: w.junior_pool_raw ?? "0",
    juniorSupplyRaw: w.junior_supply_raw ?? "0",
    activeJuniorGeneration: w.active_junior_generation ?? 0,
    redeemedRaw: w.redeemed_raw ?? "0",
    outstandingRaw: w.outstanding_raw ?? "0",
    snapshotAtMs: w.snapshot_at_ms ?? 0,
  };
}

// ═════════════════════════ pending requests (lanes) ═════════════════════════

/** One outstanding withdraw-queue request (SO-418: lane-aware, keyed by the
 * GLOBAL sequence, with server-computed payability). */
export type VaultPendingRequest = {
  /** Global sequence, decimal string — `amend_payout_asset`'s handle. */
  globalSeq: string;
  lane: LaneLabel;
  laneCode: number;
  positionId: string;
  tranche: TrancheLabel;
  trancheCode: number;
  capitalGeneration: number;
  recipient: string;
  /** u128 decimal string, atomic share units. */
  sharesRaw: string;
  /** u64 decimal string, accounting-asset smallest units. */
  basisRaw: string;
  /** Canonical coin type the recipient asked to be paid in. */
  payoutCoinType: string;
  payoutSymbol: string;
  requestedAtMs: number;
  payable: boolean;
  blockedReason: "junior_lane_blocked" | "wiped_generation" | null;
};

type VaultPendingRequestWire = {
  seq: string;
  global_seq?: string;
  // Lane/payability fields optional for rollout resilience (SO-418 adds
  // them; an untranched vault's requests all ride the junior lane).
  lane?: LaneLabel;
  lane_code?: number;
  position_id?: string;
  tranche?: TrancheLabel;
  tranche_code?: number;
  capital_generation?: number;
  recipient: string;
  shares_raw: string;
  basis_raw: string;
  payout_coin_type: string;
  payout_symbol: string;
  requested_at_ms: string;
  payable?: boolean;
  blocked_reason?: "junior_lane_blocked" | "wiped_generation" | null;
};

/** `GET /trading-vaults/:id/pending-requests` — ascending by global seq. */
export async function fetchVaultPendingRequests(
  vaultId: string,
): Promise<VaultPendingRequest[]> {
  const res = await fetch(
    `${API_BASE_URL}/trading-vaults/${encodeURIComponent(vaultId)}/pending-requests`,
  );
  if (!res.ok) {
    throw new Error(
      `GET /trading-vaults/:id/pending-requests failed: ${res.status} ${res.statusText}`,
    );
  }
  const body = (await res.json()) as { requests: VaultPendingRequestWire[] };
  return body.requests.map((r) => ({
    globalSeq: r.global_seq ?? r.seq,
    lane: r.lane ?? "junior",
    laneCode: r.lane_code ?? 1,
    positionId: r.position_id ?? "",
    tranche: r.tranche ?? "untranched",
    trancheCode: r.tranche_code ?? 0,
    capitalGeneration: r.capital_generation ?? 0,
    recipient: r.recipient,
    sharesRaw: r.shares_raw,
    basisRaw: r.basis_raw,
    payoutCoinType: r.payout_coin_type,
    payoutSymbol: r.payout_symbol,
    requestedAtMs: Number(r.requested_at_ms),
    payable: r.payable ?? true,
    blockedReason: r.blocked_reason ?? null,
  }));
}

// ═════════════════════════════ display helpers ═════════════════════════════

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

/** Displayed share price in accounting-asset units per share, or null
 * pre-appraisal. Raw pps (value/shares) carries the SO-370 virtual offset —
 * shares are 1e6× the accounting raw units — so display = raw × 1e6. */
export function tradingVaultPps(v: TradingVault): number | null {
  if (v.latestPpsE12Raw == null) return null;
  // pps_raw = nav × 1e12 × offset / shares, and a display share carries the
  // same offset (shares_raw / (10^dec × offset)), so the offsets cancel:
  // pps_raw / 1e12 IS the accounting-asset price of one display share.
  return Number(v.latestPpsE12Raw) / PPS_E12;
}

/**
 * TVL estimate in display units of the accounting asset:
 * display shares (offset divided back out) × display pps.
 */
export function tradingVaultTvl(v: TradingVault, decimals: number | null): number | null {
  const pps = tradingVaultPps(v);
  if (pps == null || decimals == null) return null;
  return (Number(v.totalSharesRaw) / (SHARE_OFFSET * 10 ** decimals)) * pps;
}

/** Per-tranche TVL estimate in display accounting-asset units, from the
 * latest TvCapitalSynced NAV split; null before the first sync. */
export function trancheTvl(navRaw: string | null, decimals: number | null): number | null {
  if (navRaw == null || decimals == null) return null;
  return Number(navRaw) / 10 ** decimals;
}

/** Human label for a risk state. */
export function riskStateLabel(s: RiskStateLabel): string {
  switch (s) {
    case "healthy":
      return "Healthy";
    case "coverage_breach":
      return "Coverage breach";
    case "impaired":
      return "Impaired";
    case "reset_pending":
      return "Reset pending";
  }
}

/** §8.4b master switch mirrored client-side: risk-off means deployment
 * stops (quote sessions, releases) while unwinding continues. */
export function isRiskOff(v: TradingVault): boolean {
  return v.riskStateCode !== 0 || v.curatorCommitmentBreached;
}
