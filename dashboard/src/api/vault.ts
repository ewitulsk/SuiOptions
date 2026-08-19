// api-service trading-vault reads. Wire types mirror the api-service v2
// DTOs (WS-3 contract, SO-418) exactly — snake_case, raw u64/u128 as
// decimal strings — so this file is a mapper, not a second source of
// truth. Domain names match frontend/src/api/tradingVaults.ts.

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

export const PPS_E12 = 1e12;

// ── v2 capital-structure vocabulary (wire codes → labels) ──────────────

/** Wire tranche label (codes: 0 untranched / 1 senior / 2 junior). */
export type TrancheLabel = "untranched" | "senior" | "junior";

/** Wire risk-state label (codes 0..3). */
export type RiskStateLabel = "healthy" | "coverage_breach" | "impaired" | "reset_pending";

/** Wire withdrawal-lane label (codes: 0 senior / 1 junior; untranched
 * vaults use the junior lane). */
export type LaneLabel = "senior" | "junior";

export type UpsideMode = "preferred_only" | "capped_participating" | "uncapped_participating";

export function trancheFromCode(code: number): TrancheLabel {
  return code === 1 ? "senior" : code === 2 ? "junior" : "untranched";
}

export function laneFromCode(code: number): LaneLabel {
  return code === 0 ? "senior" : "junior";
}

/** Immutable senior/junior terms; null on untranched vaults. */
export type VaultCapitalStructure = {
  seniorHurdleBpsAnnual: number;
  targetJuniorBps: number;
  maintenanceJuniorBps: number;
  upside: UpsideMode;
  residualParticipationBps: number;
  totalReturnCapBps: number;
};

/** A pending junior generational reset; null when none. */
export type VaultResetProposal = {
  oldGeneration: number;
  proposedAtMs: number;
  executableAtMs: number;
  recordedNavRaw: string;
  recordedSeniorClaimRaw: string;
  recordedRequiredDepositRaw: string;
};

export type LaneBounds = { head: number; tail: number };

export type TradingVault = {
  vaultId: string;
  /** The vault's unit of account (SO-370: deposits may be any allowlisted
   * asset; accounting is in this one). */
  accountingAsset: string;
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

  // ── v2 capital structure & risk state (SO-418) ──
  /** Null for untranched vaults. */
  capitalStructure: VaultCapitalStructure | null;
  riskState: RiskStateLabel;
  riskStateCode: number;
  curatorCommitmentBreached: boolean;
  /** u128 decimal strings, atomic share units (untranched supply lives in
   * junior, mirroring capital.move). */
  seniorSharesRaw: string;
  juniorSharesRaw: string;
  seniorClaimRaw: string;
  /** Waterfall NAV split from the latest TvCapitalSynced; null pre-sync. */
  seniorNavRaw: string | null;
  juniorNavRaw: string | null;
  /** Per-tranche observed pps (1e12-scaled raw string); null pre-observation. */
  seniorPpsRaw: string | null;
  juniorPpsRaw: string | null;
  /** junior_nav × 1e4 / nav from the latest sync; null before it. */
  juniorBufferBps: number | null;
  impairedSinceMs: number | null;
  activeJuniorGeneration: number;
  resetProposal: VaultResetProposal | null;
  /** Terminal settlement snapshot taken. */
  settled: boolean;
  /** Per-lane FIFO cursors (tail = next global_seq to assign, head = next
   * to fulfill). */
  laneHeads: { senior: LaneBounds; junior: LaneBounds };
};

export function isTranched(v: TradingVault): boolean {
  return v.capitalStructure != null;
}

/** The desk operator's incident question: is capital risk-off? */
export function isRiskOff(v: TradingVault): boolean {
  return v.riskStateCode !== 0 || v.curatorCommitmentBreached;
}

/** Custodied (adapter-held) position row — a vault HOLDING, not the
 * `VaultPosition` claim NFT. */
export type VaultHoldingPosition = {
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
  positions: VaultHoldingPosition[];
  balances: TradingVaultBalance[];
  balancesStale: boolean;
};

// ── wire (api-service TradingVaultDto, snake_case per WS-3 contract) ───

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

type Wire = {
  vault_id: string;
  accounting_coin_type: string;
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
  capital_structure: CapitalStructureWire | null;
  risk_state: RiskStateLabel;
  risk_state_code: number;
  curator_commitment_breached: boolean;
  senior_shares_raw: string;
  junior_shares_raw: string;
  senior_claim_raw: string;
  senior_nav_raw: string | null;
  junior_nav_raw: string | null;
  senior_pps_raw: string | null;
  junior_pps_raw: string | null;
  junior_buffer_bps: number | null;
  impaired_since_ms: number | null;
  active_junior_generation: number;
  reset_proposal: ResetProposalWire | null;
  settled: boolean;
  lane_heads: { senior: LaneBounds; junior: LaneBounds };
};

function mapVault(w: Wire): TradingVault {
  return {
    vaultId: w.vault_id,
    accountingAsset: w.accounting_coin_type,
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
    riskState: w.risk_state,
    riskStateCode: w.risk_state_code,
    curatorCommitmentBreached: w.curator_commitment_breached,
    seniorSharesRaw: w.senior_shares_raw,
    juniorSharesRaw: w.junior_shares_raw,
    seniorClaimRaw: w.senior_claim_raw,
    seniorNavRaw: w.senior_nav_raw ?? null,
    juniorNavRaw: w.junior_nav_raw ?? null,
    seniorPpsRaw: w.senior_pps_raw ?? null,
    juniorPpsRaw: w.junior_pps_raw ?? null,
    juniorBufferBps: w.junior_buffer_bps ?? null,
    impairedSinceMs: w.impaired_since_ms ?? null,
    activeJuniorGeneration: w.active_junior_generation,
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
    settled: w.settled,
    laneHeads: w.lane_heads,
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

// ── pps history (per-tranche series, v2 wire shape) ────────────────────

/** One share-price sample. Untranched vaults emit `tranche: "untranched"`;
 * `reset` marks the first junior point of a new generation (pps re-bases). */
export type PpsPoint = {
  timestampMs: number;
  tranche: TrancheLabel;
  /** Display share price (already divided down from 1e12). */
  pps: number;
  ppsRaw: string;
  /** `deposit` | `withdraw` | `capital_sync`. */
  source: string;
  reset: boolean;
};

type PpsPointWire = {
  timestamp_ms: number;
  tranche: TrancheLabel;
  pps: number;
  pps_raw: string;
  source: string;
  reset: boolean;
};

export function usePpsHistory(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["ppsHistory", urls.api, vaultId],
    queryFn: async (): Promise<PpsPoint[]> => {
      const res = await fetch(
        `${urls.api}/trading-vaults/${encodeURIComponent(vaultId as string)}/pps-history`,
      );
      if (!res.ok) throw new Error(`GET pps-history failed: ${res.status}`);
      const body = (await res.json()) as { points: PpsPointWire[] };
      return body.points.map((p) => ({
        timestampMs: p.timestamp_ms,
        tranche: p.tranche,
        pps: p.pps,
        ppsRaw: p.pps_raw,
        source: p.source,
        reset: p.reset,
      }));
    },
    enabled: Boolean(vaultId),
    refetchInterval: 60_000,
  });
}

/** TVL in accounting-asset raw units: totalShares × pps / 1e12. */
export function vaultTvlRaw(v: TradingVault): number | null {
  if (v.latestPpsE12Raw == null) return null;
  return (Number(v.totalSharesRaw) * Number(v.latestPpsE12Raw)) / PPS_E12;
}
