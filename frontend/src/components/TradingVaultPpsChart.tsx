// Share-price-over-time chart for a curated trading vault (SO-293, v2
// per-tranche SO-418).
//
// Untranched vaults draw one solid price-per-share line; tranched vaults
// draw senior and junior as two series, with vertical markers on junior
// generation resets (junior PPS re-bases to 1.0 — without a marker this
// looks like a rendering bug). Same TradingView Lightweight Charts v5 setup
// as `VaultApyChart` (themed from CSS vars, rebuilt on theme flip).
//
// §3.2 regime annotations (SO-418 visualization pass):
// - a dashed senior-hurdle reference line — where senior PPS would sit if
//   ONLY the hurdle had accrued — so the gap between senior-actual and
//   senior-hurdle is visible at a glance;
// - background shading for CoverageBreach / Impaired / ResetPending windows
//   (`regimes` prop; see `regimeShadePrimitive`).

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  LineStyle,
  createChart,
  createSeriesMarkers,
  type IChartApi,
  type ISeriesApi,
  type SeriesMarker,
  type UTCTimestamp,
} from "lightweight-charts";

import type { RiskStateLabel, TradingVaultPpsPoint, TrancheLabel } from "../api/tradingVaults";
import { useThemeMode } from "../theme";
import { RegimeShadePrimitive, type RegimeBand } from "./regimeShadePrimitive";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

/** One risk-state window to shade behind the price lines. */
export type RegimeWindow = {
  fromMs: number;
  /** Null = ongoing (shades to the chart's right edge). */
  toMs: number | null;
  kind: Exclude<RiskStateLabel, "healthy">;
};

const REGIME_COLORS: Record<RegimeWindow["kind"], string> = {
  coverage_breach: "rgba(217, 154, 43, 0.10)",
  impaired: "rgba(224, 85, 85, 0.10)",
  reset_pending: "rgba(160, 85, 224, 0.12)",
};

type Props = {
  points: TradingVaultPpsPoint[];
  loading: boolean;
  /** Accounting-asset ticker, for the axis/legend label. */
  symbol: string;
  /** Whether the vault carries a senior/junior structure — decides between
   * the single total line and the two tranche series. */
  tranched: boolean;
  /** Senior hurdle (bps/year) for the dashed reference line; null/absent
   * hides it. Only meaningful on tranched vaults. */
  hurdleBpsAnnual?: number | null;
  /** Risk-state windows to shade. Currently derived from the vault DTO's
   * live state (impaired_since_ms / reset proposal / current risk state) —
   * see the TODO where they're computed. */
  regimes?: RegimeWindow[];
};

/** Strictly increasing unique times: collapse same-second samples (keep the
 * latest) and sort ascending, as the chart library requires. */
function toLineData(points: TradingVaultPpsPoint[]): { time: UTCTimestamp; value: number }[] {
  const bySecond = new Map<number, number>();
  for (const p of [...points].sort((a, b) => a.timestampMs - b.timestampMs)) {
    bySecond.set(Math.floor(p.timestampMs / 1000), p.pps);
  }
  return [...bySecond.entries()].map(([time, value]) => ({ time: time as UTCTimestamp, value }));
}

const SECONDS_PER_YEAR = 31_536_000;

/**
 * The hurdle-only senior PPS reference: starting from the first observed
 * senior sample, accrue the hurdle piecewise at every subsequent sample time
 * (`ref += ref × h × Δt / year`) — mirroring §2's accrue-at-every-mutation
 * rule, which compounds the claim at each capital event rather than on
 * principal alone.
 */
function hurdleLineData(
  senior: { time: UTCTimestamp; value: number }[],
  hurdleBpsAnnual: number,
): { time: UTCTimestamp; value: number }[] {
  if (senior.length < 2) return [];
  let ref = senior[0].value;
  const out = [{ time: senior[0].time, value: ref }];
  for (let i = 1; i < senior.length; i++) {
    const dt = (senior[i].time as number) - (senior[i - 1].time as number);
    ref *= 1 + (hurdleBpsAnnual / 10_000) * (dt / SECONDS_PER_YEAR);
    out.push({ time: senior[i].time, value: ref });
  }
  return out;
}

export function TradingVaultPpsChart({
  points,
  loading,
  symbol,
  tranched,
  hurdleBpsAnnual,
  regimes,
}: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<Map<TrancheLabel, ISeriesApi<"Line">> | null>(null);
  const hurdleRef = useRef<ISeriesApi<"Line"> | null>(null);
  const regimeRef = useRef<RegimeShadePrimitive | null>(null);
  const showHurdle = tranched && hurdleBpsAnnual != null && hurdleBpsAnnual > 0;

  // Rebuild on theme flip — Lightweight Charts reads colors once. Also
  // rebuild when the vault's structure resolves, since the series set
  // differs.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const accent = cssVar("--aqua-accent", "#2f81f7");
    const up = cssVar("--aqua-up", "#1fbf75");

    const chart = createChart(el, {
      height: 220,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: ink,
        fontFamily: "inherit",
        attributionLogo: false,
      },
      grid: { vertLines: { color: grid }, horzLines: { color: grid } },
      // A vertical touch drag scrolls the page, not the chart's price scale —
      // otherwise the chart traps the scroll on mobile.
      handleScroll: { vertTouchDrag: false },
      rightPriceScale: { borderVisible: false },
      timeScale: { borderVisible: false, timeVisible: true, secondsVisible: false },
    });
    const mkLine = (color: string) =>
      chart.addSeries(LineSeries, {
        color,
        lineWidth: 2,
        priceLineVisible: false,
        lastValueVisible: true,
        priceFormat: {
          type: "custom",
          formatter: (v: number) => v.toFixed(4),
        },
      });

    const series = new Map<TrancheLabel, ISeriesApi<"Line">>();
    if (tranched) {
      series.set("senior", mkLine(up));
      series.set("junior", mkLine(accent));
    } else {
      series.set("untranched", mkLine(accent));
    }

    // Dashed hurdle reference under the senior line.
    if (showHurdle) {
      hurdleRef.current = chart.addSeries(LineSeries, {
        color: up,
        lineWidth: 1,
        lineStyle: LineStyle.Dashed,
        priceLineVisible: false,
        lastValueVisible: false,
        crosshairMarkerVisible: false,
        priceFormat: { type: "custom", formatter: (v: number) => v.toFixed(4) },
      });
    }

    // Regime background shading rides the first price series' pane.
    const host = series.values().next().value as ISeriesApi<"Line"> | undefined;
    if (host) {
      const primitive = new RegimeShadePrimitive();
      host.attachPrimitive(primitive);
      regimeRef.current = primitive;
    }

    chartRef.current = chart;
    seriesRef.current = series;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
      hurdleRef.current = null;
      regimeRef.current = null;
    };
  }, [mode, tranched, showHurdle]);

  useEffect(() => {
    const series = seriesRef.current;
    if (!series) return;
    // Below two points the empty-state overlay is showing — plot nothing, or
    // a dead one-point line (and its axis price tag) draws through the text.
    const usable = points.length < 2 ? [] : points;
    for (const [tranche, line] of series) {
      const mine = usable.filter((p) => p.tranche === tranche);
      const data = toLineData(mine);
      line.setData(data);
      if (tranche === "senior" && hurdleRef.current) {
        hurdleRef.current.setData(
          hurdleBpsAnnual != null ? hurdleLineData(data, hurdleBpsAnnual) : [],
        );
      }
      // Junior generation-reset markers: the pps re-base is deliberate.
      if (tranche === "junior") {
        const markers: SeriesMarker<UTCTimestamp>[] = mine
          .filter((p) => p.reset)
          .sort((a, b) => a.timestampMs - b.timestampMs)
          .map((p) => ({
            time: Math.floor(p.timestampMs / 1000) as UTCTimestamp,
            position: "aboveBar",
            color: cssVar("--aqua-down", "#e05555"),
            shape: "arrowDown",
            text: "reset",
          }));
        createSeriesMarkers(line, markers);
      }
    }
    // Shade risk-state windows behind the lines (skipped alongside the
    // empty-state overlay).
    regimeRef.current?.setBands(
      usable.length === 0
        ? []
        : (regimes ?? []).map(
            (r): RegimeBand => ({
              from: Math.floor(r.fromMs / 1000) as UTCTimestamp,
              to: r.toMs != null ? (Math.floor(r.toMs / 1000) as UTCTimestamp) : null,
              color: REGIME_COLORS[r.kind],
            }),
          ),
    );
    chartRef.current?.timeScale().fitContent();
  }, [points, mode, tranched, hurdleBpsAnnual, regimes]);

  const empty = points.length < 2;

  return (
    <div className="vault-card vault-chart">
      <div className="vault-card__head">
        Share price over time
        {tranched && (
          <span className="vault-prose__muted" style={{ marginLeft: "auto", fontSize: 11 }}>
            <span style={{ color: "var(--aqua-up, #1fbf75)" }}>— senior</span>
            {showHurdle && (
              <span style={{ color: "var(--aqua-up, #1fbf75)", marginLeft: 8, opacity: 0.7 }}>
                ┄ hurdle
              </span>
            )}
            <span style={{ color: "var(--aqua-accent, #2f81f7)", marginLeft: 8 }}>— junior</span>
          </span>
        )}
      </div>
      <div className="vault-chart__holder">
        <div ref={holderRef} style={{ width: "100%", height: 220 }} />
        {!loading && empty && (
          <div className="vault-chart__empty">
            <div className="vault-chart__empty-title">No share-price history yet</div>
            <div className="vault-chart__empty-sub">
              The vault records a share price at each appraisal — deposits,
              withdrawal fulfillments, and capital syncs. The curve fills in
              here as the vault operates.
            </div>
          </div>
        )}
      </div>
      <div className="vault-card__foot vault-prose__muted">
        {tranched
          ? "Per-tranche price per share in " +
            symbol +
            ". Markers flag junior generation resets (junior re-bases to 1.0)" +
            (showHurdle ? "; the dashed line is senior at hurdle-only accrual" : "") +
            "; shaded windows are risk-off states."
          : `Price per share in ${symbol}, from on-chain appraisals.`}
      </div>
    </div>
  );
}
