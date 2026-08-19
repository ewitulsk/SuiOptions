// §3.1 waterfall explorer (SO-418) — the centerpiece of the tranched-vault
// detail page. Renders the live capital stack from the `/waterfall` payload
// and a what-if slider on total NAV (−60%…+60%) that re-runs the spec-§3.4a
// waterfall CLIENT-SIDE (lib/waterfall — bigint, floor division) and
// animates the stack: junior absorbs the first loss, senior only starts
// losing after junior hits zero, and in participating modes senior's slice
// keeps growing past the claim. The two break points render directly on the
// slider track.
//
// The simulation holds the senior claim, principal basis, and supplies
// constant — it moves NAV only, which is exactly what the §3.4a waterfall is
// a pure function of.

import { useState } from "react";

import type { VaultWaterfall } from "../api/tradingVaults";
import { formatPrice } from "../format";
import { waterfallBreakpoints } from "../lib/waterfall";
import { HowTranchesWork } from "./TrancheEducation";
import { WaterfallStackBar } from "./WaterfallStackBar";

const SLIDER_MIN = -60;
const SLIDER_MAX = 60;

function modeCaption(w: VaultWaterfall): string {
  switch (w.upside) {
    case "preferred_only":
      return "Preferred only: senior upside stops at its claim — junior owns the entire residual.";
    case "capped_participating":
      return (
        `Capped participating: senior takes ${(w.residualParticipationBps / 100).toFixed(0)}% ` +
        `of the residual, up to ${(w.totalReturnCapBps / 100).toFixed(0)}% total return on its principal.`
      );
    case "uncapped_participating":
      return `Uncapped participating: senior takes ${(w.residualParticipationBps / 100).toFixed(0)}% of every residual gain.`;
  }
}

export function WaterfallExplorer({
  waterfall: w,
  symbol,
  decimals,
  termsVersion,
}: {
  waterfall: VaultWaterfall;
  symbol: string;
  decimals: number | null;
  termsVersion: number;
}) {
  const [deltaPct, setDeltaPct] = useState(0);

  const nav = BigInt(w.navRaw);
  const claim = BigInt(w.seniorClaimRaw);
  const principal = w.seniorPrincipalBasisRaw != null ? BigInt(w.seniorPrincipalBasisRaw) : null;
  const simNav = (nav * BigInt(100 + deltaPct)) / 100n;
  const simArrears = claim > simNav;

  // Break points as slider positions: the NAV level where junior is wiped
  // (NAV = C — senior arrears begin at the same kink) and, when the
  // principal basis is known, where senior starts losing principal
  // (NAV = P < C). Rendered only when they fall inside the slider range.
  const bps = waterfallBreakpoints(claim, principal);
  const toDelta = (level: bigint): number | null => {
    if (nav <= 0n) return null;
    const d = Number((level * 10_000n) / nav) / 100 - 100;
    return d >= SLIDER_MIN && d <= SLIDER_MAX ? d : null;
  };
  const marks: { delta: number; label: string }[] = [];
  const juniorWipedDelta = toDelta(bps.juniorWiped);
  if (juniorWipedDelta != null) marks.push({ delta: juniorWipedDelta, label: "junior wiped" });
  const principalDelta = bps.seniorPrincipal != null ? toDelta(bps.seniorPrincipal) : null;
  if (principalDelta != null) marks.push({ delta: principalDelta, label: "senior impaired" });

  const fmt = (raw: bigint): string =>
    decimals != null
      ? `${formatPrice(Number(raw) / 10 ** decimals, { grouping: true })} ${symbol}`
      : raw.toString();

  if (nav <= 0n) {
    return (
      <div className="vault-card">
        <div className="vault-card__head">Waterfall</div>
        <div className="vault-card__body vault-prose__muted">
          No capital sync yet — the stack fills in after the first consumed
          appraisal.
        </div>
      </div>
    );
  }

  return (
    <div className="vault-card">
      <div className="vault-card__head" style={{ display: "flex", alignItems: "center" }}>
        Waterfall explorer
        <span style={{ marginLeft: "auto" }}>
          <HowTranchesWork autoOpen termsVersion={termsVersion} />
        </span>
      </div>

      <WaterfallStackBar
        inputs={{
          mode: w.upside,
          nav: simNav,
          claim,
          principal,
          participationBps: w.residualParticipationBps,
          capBps: w.totalReturnCapBps,
        }}
        symbol={symbol}
        decimals={decimals}
      />

      <div style={{ marginTop: 14 }}>
        <div
          style={{
            display: "flex",
            alignItems: "baseline",
            gap: 8,
            fontSize: 12,
            marginBottom: 2,
          }}
        >
          <span>
            What if NAV {deltaPct === 0 ? "stayed at" : deltaPct > 0 ? "rose to" : "fell to"}{" "}
            <b>{fmt(simNav)}</b>
            {deltaPct !== 0 && (
              <span className="vault-bids__sub">
                {" "}
                ({deltaPct > 0 ? "+" : ""}
                {deltaPct}%)
              </span>
            )}
          </span>
          {deltaPct !== 0 && (
            <button
              className="vault-howto__trigger"
              style={{ marginLeft: "auto", fontSize: 10 }}
              onClick={() => setDeltaPct(0)}
            >
              reset
            </button>
          )}
        </div>
        <input
          type="range"
          min={SLIDER_MIN}
          max={SLIDER_MAX}
          step={1}
          value={deltaPct}
          onChange={(e) => setDeltaPct(Number(e.target.value))}
          style={{ width: "100%", display: "block" }}
          aria-label="Simulated NAV change, percent"
        />
        {/* Break-point annotations on the slider track. */}
        {marks.length > 0 && (
          <div style={{ position: "relative", height: 26 }}>
            {marks.map((m) => {
              const leftPct = ((m.delta - SLIDER_MIN) / (SLIDER_MAX - SLIDER_MIN)) * 100;
              return (
                <span
                  key={m.label}
                  style={{
                    position: "absolute",
                    left: `${leftPct}%`,
                    top: 0,
                    transform: "translateX(-50%)",
                    textAlign: "center",
                    color: "var(--aqua-down, #e05555)",
                    fontSize: 9,
                    lineHeight: 1.2,
                    whiteSpace: "nowrap",
                  }}
                >
                  <span
                    aria-hidden
                    style={{
                      display: "block",
                      width: 1,
                      height: 7,
                      background: "currentcolor",
                      margin: "0 auto 1px",
                    }}
                  />
                  {m.label}
                </span>
              );
            })}
          </div>
        )}
      </div>

      <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 4 }}>
        {modeCaption(w)}
      </div>
      {simArrears && (
        <div
          className="vault-prose__muted"
          style={{ fontSize: 11, marginTop: 4, color: "var(--aqua-down, #e05555)" }}
        >
          The hatched slice is arrears — senior claim the vault cannot fund.
          The claim keeps accruing during impairment and absorbs later
          recovery first.
        </div>
      )}
      <div className="vault-card__foot vault-prose__muted">
        Simulated client-side with the exact on-chain waterfall; the senior
        claim ({fmt(claim)}) is held constant while NAV moves.
      </div>
    </div>
  );
}
