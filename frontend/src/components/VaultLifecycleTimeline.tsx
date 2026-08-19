// §3.7 lifecycle timeline (SO-418): a vertical event history for a trading
// vault — created → (breach) → impaired → reset proposed → executed →
// closing → settled — assembled from what the API exposes today:
// pps-history (first activity ≈ creation; reset-flagged points = executed
// junior resets), the vault DTO (impaired_since_ms, reset proposal, risk
// state, lifecycle state), and the settlement payload (snapshot time).
//
// TODO(SO-418): historical CoverageBreach / cured windows (and exact
// created/closing/closed timestamps) need an indexer state-transition
// series; until it exists those render as undated current-state entries and
// the breach history is invisible once cured.

import type {
  TradingVault,
  TradingVaultPpsPoint,
  VaultSettlement,
} from "../api/tradingVaults";

type Tone = "ok" | "warn" | "bad" | "muted";

type TimelineEvent = {
  /** Null = no timestamp available (current-state entry). */
  atMs: number | null;
  label: string;
  detail?: string;
  tone: Tone;
  /** True for a scheduled future step (reset executable). */
  future?: boolean;
};

const TONE_COLOR: Record<Tone, string> = {
  ok: "var(--aqua-up, #1fbf75)",
  warn: "#d99a2b",
  bad: "var(--aqua-down, #e05555)",
  muted: "var(--aqua-ink-3, #8896a5)",
};

function fmtWhen(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

function fmtCountdown(ms: number): string {
  if (ms <= 0) return "now";
  const hours = ms / 3_600_000;
  if (hours < 1) return `in ${Math.max(1, Math.round(ms / 60_000))}m`;
  if (hours < 48) return `in ${Math.round(hours)}h`;
  return `in ${Math.round(hours / 24)}d`;
}

function buildEvents(
  vault: TradingVault,
  points: TradingVaultPpsPoint[],
  settlement: VaultSettlement | null,
): TimelineEvent[] {
  const now = Date.now();
  const dated: TimelineEvent[] = [];
  const undated: TimelineEvent[] = [];
  const future: TimelineEvent[] = [];

  const firstMs = points.reduce<number | null>(
    (min, p) => (min == null || p.timestampMs < min ? p.timestampMs : min),
    null,
  );
  if (firstMs != null) {
    dated.push({
      atMs: firstMs,
      label: "Created",
      detail: "first recorded activity",
      tone: "muted",
    });
  }

  for (const p of points) {
    if (!p.reset) continue;
    dated.push({
      atMs: p.timestampMs,
      label: "Junior reset executed",
      detail: "old junior generation permanently wiped; junior PPS re-based to 1.0",
      tone: "bad",
    });
  }

  if (vault.impairedSinceMs != null) {
    dated.push({
      atMs: vault.impairedSinceMs,
      label: "Impaired",
      detail: "junior wiped; assets below the senior claim — ordinary deposits stopped",
      tone: "bad",
    });
  }

  const proposal = vault.resetProposal;
  if (proposal != null) {
    dated.push({
      atMs: proposal.proposedAtMs,
      label: `Junior reset proposed (gen ${proposal.oldGeneration} → ${proposal.oldGeneration + 1})`,
      detail: "recovery before execution cancels it automatically",
      tone: "bad",
    });
    future.push({
      atMs: proposal.executableAtMs,
      label:
        proposal.executableAtMs > now
          ? `Reset executable ${fmtCountdown(proposal.executableAtMs - now)}`
          : "Reset executable now",
      detail: "the final required deposit is recomputed at execution",
      tone: "warn",
      future: true,
    });
  }

  if (vault.state !== "closed" && vault.riskState === "coverage_breach") {
    undated.push({
      atMs: null,
      label: "Coverage breach (ongoing)",
      detail:
        "junior buffer below maintenance — junior withdrawals paused in place, senior keeps flowing",
      tone: "warn",
    });
  }

  if (vault.state === "closing") {
    undated.push({
      atMs: null,
      label: "Closing (ongoing)",
      detail: "deposits stopped; the curator unwinds positions",
      tone: "warn",
    });
  }
  if (vault.state === "closed") {
    undated.push({
      atMs: null,
      label: "Closed",
      detail: settlement?.settled
        ? undefined
        : "awaiting the one-time settlement snapshot",
      tone: "muted",
    });
  }
  if (settlement?.settled) {
    dated.push({
      atMs: settlement.snapshotAtMs,
      label: "Settled",
      detail:
        "entitlements frozen, senior first — positions redeem against the pool at any later time",
      tone: "ok",
    });
  }

  dated.sort((a, b) => (a.atMs ?? 0) - (b.atMs ?? 0));
  return [...dated, ...undated, ...future];
}

export function VaultLifecycleTimeline({
  vault,
  points,
  settlement,
}: {
  vault: TradingVault;
  points: TradingVaultPpsPoint[];
  settlement: VaultSettlement | null;
}) {
  const events = buildEvents(vault, points, settlement);
  if (events.length === 0) return null;

  return (
    <div className="vault-card">
      <div className="vault-card__head">Lifecycle</div>
      <div
        style={{
          borderLeft: "2px solid var(--aqua-line, rgba(92,107,122,0.2))",
          marginLeft: 5,
          paddingLeft: 16,
          display: "grid",
          gap: 12,
        }}
      >
        {events.map((e, i) => (
          <div key={`${e.label}-${e.atMs ?? i}`} style={{ position: "relative" }}>
            <span
              aria-hidden
              style={{
                position: "absolute",
                left: -22,
                top: 4,
                width: 9,
                height: 9,
                borderRadius: "50%",
                background: e.future ? "transparent" : TONE_COLOR[e.tone],
                border: `2px solid ${TONE_COLOR[e.tone]}`,
                boxSizing: "border-box",
              }}
            />
            <div style={{ fontSize: 12 }}>
              {e.label}
              {e.atMs != null && !e.future && (
                <span className="vault-bids__sub"> · {fmtWhen(e.atMs)}</span>
              )}
            </div>
            {e.detail && (
              <div className="vault-prose__muted" style={{ fontSize: 11 }}>
                {e.detail}
              </div>
            )}
          </div>
        ))}
      </div>
      <div className="vault-card__foot vault-prose__muted">
        Assembled from on-chain activity; undated entries reflect the current
        state.
      </div>
    </div>
  );
}
