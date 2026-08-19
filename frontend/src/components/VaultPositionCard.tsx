// One `VaultPosition` claim NFT rendered as a card (SO-418, plan §3.4):
// tranche badge, shares, cost basis, current estimated value, the EMBEDDED
// performance-fee liability (est. fee if exited now — a secondary buyer
// inherits it), lock countdown, and the junior generation with a prominent
// "Wiped — permanently zero" treatment for stale generations.
//
// Shared between the vault detail's positions panel and the standalone
// `/vaults/:vaultId/positions/:positionId` due-diligence page — the same
// card a prospective secondary buyer sees pre-purchase. Presentational:
// action rows are passed as children.

import { SHARE_OFFSET, type VaultPosition } from "../api/tradingVaults";
import { formatPrice } from "../format";
import { HowTranchesWork } from "./TrancheEducation";

// TODO(SO-418 §3.4): lineage link (split-from / merged-from). Neither the
// positions endpoints nor the position object expose parent ids today — a
// lineage read needs the indexer's split/merge events surfaced per position.

function fmtDurationMs(ms: number): string {
  if (ms <= 0) return "none";
  const hours = ms / 3_600_000;
  if (hours < 1) return `${Math.max(1, Math.round(ms / 60_000))}m`;
  if (hours < 48) return `${Math.round(hours)}h`;
  return `${Math.round(hours / 24)}d`;
}

export function positionShares(p: VaultPosition, decimals: number | null): string {
  return decimals != null
    ? formatPrice(Number(p.sharesRaw) / (10 ** decimals * SHARE_OFFSET), { grouping: true })
    : p.sharesRaw;
}

function amount(raw: string | null, decimals: number | null, symbol: string): string {
  if (raw == null) return "—";
  return decimals != null
    ? `${formatPrice(Number(raw) / 10 ** decimals, { grouping: true })} ${symbol}`
    : raw;
}

export function VaultPositionCard({
  position: p,
  symbol,
  decimals,
  children,
}: {
  position: VaultPosition;
  /** Accounting-asset ticker of the position's vault. */
  symbol: string;
  decimals: number | null;
  /** Optional action row(s), rendered below the stats. */
  children?: React.ReactNode;
}) {
  const now = Date.now();
  const lockedMs = p.lockedUntilMs > now ? p.lockedUntilMs - now : null;
  const trancheColor =
    p.tranche === "senior"
      ? "var(--aqua-up, #1fbf75)"
      : p.tranche === "junior"
        ? "var(--aqua-accent, #2f81f7)"
        : "var(--aqua-ink-3)";

  return (
    <div
      className="vault-card"
      style={{
        marginBottom: 10,
        ...(p.wiped ? { borderColor: "var(--aqua-down, #e05555)" } : {}),
      }}
    >
      <div className="vault-card__head" style={{ display: "flex", alignItems: "center", gap: 6 }}>
        <span
          className="vault-head__badge"
          style={{ color: trancheColor, borderColor: "currentcolor", textTransform: "capitalize" }}
        >
          {p.tranche}
        </span>
        {p.tranche === "junior" && (
          <span className="vault-bids__sub">gen {p.capitalGeneration}</span>
        )}
        {p.tranche !== "untranched" && <HowTranchesWork compact />}
        <span
          className="vault-bids__sub"
          style={{ marginLeft: "auto" }}
          title={p.positionId}
        >
          {p.positionId.slice(0, 6)}…{p.positionId.slice(-4)}
        </span>
      </div>
      {p.wiped && (
        <div
          className="dash-alert"
          role="alert"
          style={{ margin: "6px 0", color: "var(--aqua-down, #e05555)" }}
        >
          <strong>Wiped — permanently zero.</strong> This junior position
          belongs to generation {p.capitalGeneration}, before a completed
          reset. It can never regain value, even if NAV recovers; it redeems
          at zero and can be burned.
        </div>
      )}
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Shares</span>
          <span>{positionShares(p, decimals)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Cost basis</span>
          <span>{amount(p.costBasisRaw, decimals, symbol)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Est. value</span>
          <span>{p.wiped ? `0 ${symbol}` : amount(p.estimatedValueRaw, decimals, symbol)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Embedded fee if exited now</span>
          <span>{p.wiped ? `0 ${symbol}` : amount(p.estimatedFeeRaw, decimals, symbol)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Lockup</span>
          <span>{lockedMs != null ? `unlocks in ${fmtDurationMs(lockedMs)}` : "unlocked"}</span>
        </div>
      </div>
      {children}
    </div>
  );
}
