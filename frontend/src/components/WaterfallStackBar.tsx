// The §3.1 capital-stack bar (SO-418): one horizontal bar splitting a total
// NAV into senior preferred (principal + accrued hurdle visually separated),
// senior participation (participating modes), and junior residual. Under
// impairment the unfunded senior claim renders as a hatched "arrears"
// segment extending past the NAV boundary marker.
//
// Purely presentational and driven by explicit inputs so the same component
// serves the live waterfall explorer, its what-if simulation, and the §3.8
// education flow's canned scenario. Splits are computed by the shared
// `lib/waterfall` mirror of spec §3.4a — never ad hoc.

import type { UpsideMode } from "../api/tradingVaults";
import { formatPrice } from "../format";
import { waterfall } from "../lib/waterfall";

export type StackInputs = {
  mode: UpsideMode;
  /** Accounting-asset smallest units. */
  nav: bigint;
  claim: bigint;
  /** Senior principal basis P; null when unknown. */
  principal: bigint | null;
  participationBps: number;
  capBps: number;
};

const COLOR_PRINCIPAL = "var(--aqua-up, #1fbf75)";
const COLOR_HURDLE =
  "repeating-linear-gradient(135deg, rgba(31,191,117,0.9) 0 5px, rgba(31,191,117,0.45) 5px 10px)";
const COLOR_PARTICIPATION = "rgba(31,191,117,0.35)";
const COLOR_JUNIOR = "var(--aqua-accent, #2f81f7)";
const COLOR_ARREARS =
  "repeating-linear-gradient(135deg, rgba(224,85,85,0.6) 0 5px, rgba(224,85,85,0.12) 5px 10px)";

function fmtRaw(raw: bigint, decimals: number | null, symbol: string): string {
  if (decimals == null) return raw.toString();
  const n = formatPrice(Number(raw) / 10 ** decimals, { grouping: true });
  return symbol ? `${n} ${symbol}` : n;
}

function Dot({ background }: { background: string }) {
  return (
    <span
      aria-hidden
      style={{
        display: "inline-block",
        width: 9,
        height: 9,
        borderRadius: 2,
        background,
        flex: "0 0 auto",
      }}
    />
  );
}

export function WaterfallStackBar({
  inputs,
  symbol,
  decimals,
}: {
  inputs: StackInputs;
  symbol: string;
  decimals: number | null;
}) {
  const { mode, nav, claim, principal, participationBps, capBps } = inputs;
  const split = waterfall(mode, nav, claim, principal ?? 0n, participationBps, capBps);

  // Preferred = principal part + accrued-hurdle part (visually separated when
  // P is known; a single segment otherwise).
  const principalPart =
    principal != null && principal < split.preferred ? principal : split.preferred;
  const hurdlePart = split.preferred - principalPart;
  // Unfunded claim under impairment: extends the bar past the NAV marker.
  const arrears = claim > nav ? claim - nav : 0n;
  const span = nav + arrears;

  if (span <= 0n) {
    return (
      <div className="vault-prose__muted" style={{ fontSize: 12 }}>
        NAV is zero — nothing to distribute.
      </div>
    );
  }

  const pct = (x: bigint) => Number((x * 100_000n) / span) / 1000;
  const seg = (width: bigint, background: string, title: string, dashed = false) =>
    width > 0n ? (
      <div
        key={title}
        title={title}
        style={{
          width: `${pct(width)}%`,
          background,
          transition: "width .25s ease",
          minWidth: 0,
          ...(dashed ? { outline: "1px dashed var(--aqua-down, #e05555)", outlineOffset: -1 } : {}),
        }}
      />
    ) : null;

  const segments = [
    seg(
      principalPart,
      COLOR_PRINCIPAL,
      `Senior preferred — principal: ${fmtRaw(principalPart, decimals, symbol)}`,
    ),
    seg(
      hurdlePart,
      COLOR_HURDLE,
      `Senior preferred — accrued hurdle: ${fmtRaw(hurdlePart, decimals, symbol)}`,
    ),
    seg(
      split.participation,
      COLOR_PARTICIPATION,
      `Senior participation: ${fmtRaw(split.participation, decimals, symbol)}`,
    ),
    seg(split.juniorNav, COLOR_JUNIOR, `Junior residual: ${fmtRaw(split.juniorNav, decimals, symbol)}`),
    seg(
      arrears,
      COLOR_ARREARS,
      `Arrears — unfunded senior claim: ${fmtRaw(arrears, decimals, symbol)}`,
      true,
    ),
  ];

  return (
    <div>
      <div style={{ position: "relative" }}>
        <div
          style={{
            display: "flex",
            height: 28,
            borderRadius: 6,
            overflow: "hidden",
            border: "1px solid var(--aqua-line, rgba(92,107,122,0.2))",
          }}
        >
          {segments}
        </div>
        {arrears > 0n && (
          // The NAV boundary: everything right of this line is claim the
          // vault cannot currently fund.
          <div
            aria-hidden
            style={{
              position: "absolute",
              top: -3,
              bottom: -3,
              left: `${pct(nav)}%`,
              width: 2,
              background: "var(--aqua-ink-1, #1c2733)",
              opacity: 0.65,
              transition: "left .25s ease",
            }}
            title={`NAV boundary: ${fmtRaw(nav, decimals, symbol)}`}
          />
        )}
      </div>
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: "4px 14px",
          marginTop: 8,
          fontSize: 11,
          alignItems: "center",
        }}
      >
        <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
          <Dot background={COLOR_PRINCIPAL} /> senior principal{" "}
          {fmtRaw(principalPart, decimals, symbol)}
        </span>
        {hurdlePart > 0n && (
          <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
            <Dot background={COLOR_HURDLE} /> accrued hurdle {fmtRaw(hurdlePart, decimals, symbol)}
          </span>
        )}
        {split.participation > 0n && (
          <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
            <Dot background={COLOR_PARTICIPATION} /> senior participation{" "}
            {fmtRaw(split.participation, decimals, symbol)}
          </span>
        )}
        <span style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
          <Dot background={COLOR_JUNIOR} /> junior residual{" "}
          {fmtRaw(split.juniorNav, decimals, symbol)}
        </span>
        {arrears > 0n && (
          <span
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 5,
              color: "var(--aqua-down, #e05555)",
            }}
          >
            <Dot background={COLOR_ARREARS} /> arrears {fmtRaw(arrears, decimals, symbol)}
          </span>
        )}
      </div>
    </div>
  );
}
