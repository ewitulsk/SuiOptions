// DEPRECATED (SO-332) — APY chart for the retired covered-call vaults.
// Only screens/Vault.tsx rendered it, and that page is unmounted.
//
// APY-over-time chart for a vault.
//
// Two series share the panel (TradingView Lightweight Charts v5, themed from
// CSS vars, like `ChartPanel`): a SOLID line for realized APY (annualized pps
// growth per finalized round, from the indexer) and a DASHED line for
// predicted APY (forward premium-yield, from price-charting's apy sampler). The
// dashed line is anchored to the last realized point so it visually continues
// the curve into the current/future rounds.

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  LineStyle,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";

import type { VaultApyPoint } from "../api/vaults";
import { ApyBandPrimitive, type BandPoint } from "./apyBandPrimitive";
import { useThemeMode } from "../theme";

/** Translucent rgba from a #rrggbb / #rgb color, with an accent fallback. */
function rgba(color: string, alpha: number): string {
  const hex = color.trim().replace(/^#/, "");
  const full = hex.length === 3 ? hex.split("").map((c) => c + c).join("") : hex;
  if (!/^[0-9a-f]{6}$/i.test(full)) return `rgba(47,129,247,${alpha})`;
  const n = parseInt(full, 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${alpha})`;
}

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function pct(x: number | null | undefined, digits = 1): string {
  if (x == null || !Number.isFinite(x)) return "—";
  return `${(x * 100).toFixed(digits)}%`;
}

type Props = {
  realized: VaultApyPoint[];
  predicted: VaultApyPoint[];
  loading: boolean;
};

export function VaultApyChart({ realized, predicted, loading }: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const realizedRef = useRef<ISeriesApi<"Line"> | null>(null);
  const predictedRef = useRef<ISeriesApi<"Line"> | null>(null);
  const bandRef = useRef<ApyBandPrimitive | null>(null);

  // Rebuild on theme flip — Lightweight Charts reads colors once.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const up = cssVar("--aqua-up", "#1fbf75");
    const accent = cssVar("--aqua-accent", "#2f81f7");
    const pctFmt = {
      type: "custom" as const,
      formatter: (v: number) => `${(v * 100).toFixed(1)}%`,
    };

    const chart = createChart(el, {
      height: 260,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: ink,
        fontFamily: "inherit",
        attributionLogo: false,
      },
      grid: { vertLines: { color: grid }, horzLines: { color: grid } },
      rightPriceScale: { borderVisible: false },
      timeScale: { borderVisible: false, timeVisible: false, secondsVisible: false },
    });
    const realizedLine = chart.addSeries(LineSeries, {
      color: up,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: pctFmt,
    });
    const predictedLine = chart.addSeries(LineSeries, {
      color: accent,
      lineWidth: 2,
      lineStyle: LineStyle.Dashed,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: pctFmt,
    });

    // Confidence-band fill behind the projected line, attached to the predicted
    // series so it shares its price scale.
    const band = new ApyBandPrimitive();
    predictedLine.attachPrimitive(band);

    chartRef.current = chart;
    realizedRef.current = realizedLine;
    predictedRef.current = predictedLine;
    bandRef.current = band;
    return () => {
      chart.remove();
      chartRef.current = null;
      realizedRef.current = null;
      predictedRef.current = null;
      bandRef.current = null;
    };
  }, [mode]);

  useEffect(() => {
    const rl = realizedRef.current;
    const pl = predictedRef.current;
    if (!rl || !pl) return;
    const toLine = (pts: VaultApyPoint[]) =>
      pts
        .map((p) => ({ time: (p.t_ms / 1000) as UTCTimestamp, value: p.apy }))
        .sort((a, b) => (a.time as number) - (b.time as number));
    rl.setData(toLine(realized));
    // Anchor the dashed line to the last realized point so it continues the
    // curve rather than floating disconnected.
    const last = realized.length ? [realized[realized.length - 1]] : [];
    pl.setData(toLine([...last, ...predicted]));

    // Band: same anchored segment, pinched at the realized anchor (low=high=apy
    // there, since realized points carry no band) and opening to the forecast
    // range. Hidden when the band is degenerate.
    const band = bandRef.current;
    if (band) {
      const bandData: BandPoint[] = [...last, ...predicted]
        .map((p) => ({
          time: (p.t_ms / 1000) as UTCTimestamp,
          low: p.apy_low ?? p.apy,
          high: p.apy_high ?? p.apy,
        }))
        .sort((a, b) => (a.time as number) - (b.time as number));
      const hasBand = bandData.some((d) => d.high - d.low > 1e-4);
      const accent = cssVar("--aqua-accent", "#2f81f7");
      band.setData(hasBand ? bandData : [], rgba(accent, 0.16));
    }
  }, [realized, predicted]);

  const empty = realized.length === 0 && predicted.length === 0;

  // Latest predicted point carries the per-round assignment risk. Most rounds
  // the call expires worthless and the vault keeps the premium; in the
  // assignment_prob share of rounds it's assigned and gives up the downside.
  const proj = predicted.length
    ? predicted.reduce((a, b) => (b.t_ms > a.t_ms ? b : a))
    : null;
  const assignProb = proj?.assignment_prob;
  const downside = proj?.downside_round_yield;
  const hasRisk = assignProb != null && downside != null;

  return (
    <div className="vault-card vault-chart">
      <div
        className="vault-card__head"
        style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}
      >
        <span>
          
          APY over time
        </span>
        <span className="vault-chart__legend">
          <span className="vault-chart__legend-item">
            <span className="vault-chart__swatch vault-chart__swatch--realized" /> realized
          </span>
          <span className="vault-chart__legend-item">
            <span className="vault-chart__swatch vault-chart__swatch--predicted" /> projected
          </span>
        </span>
      </div>
      <div className="vault-chart__holder">
        <div ref={holderRef} style={{ width: "100%", height: 260 }} />
        {!loading && empty && (
          <div className="vault-chart__empty">
            <div className="vault-chart__empty-title">APY history coming soon</div>
            <div className="vault-chart__empty-sub">
              The strategy publishes a realized APY each time a round finalizes,
              and a projected APY for the round in progress. The curves fill in
              here as rounds settle.
            </div>
          </div>
        )}
      </div>
      <div className="vault-card__foot vault-prose__muted">
        Projected (dashed) is a model estimate, not a guarantee — it's net of
        the average assignment cost, but a sharp rally can assign the call and
        cost more than the average in any single round.
      </div>
      {hasRisk && (
        <div className="vault-chart__risk">
          <div className="vault-chart__risk-item">
            <span className="vault-chart__risk-val">{pct(assignProb)}</span>
            <span className="vault-chart__risk-label">assignment chance / round</span>
          </div>
          <div className="vault-chart__risk-item">
            <span className="vault-chart__risk-val is-neg">{pct(downside)}</span>
            <span className="vault-chart__risk-label">round yield if assigned</span>
          </div>
        </div>
      )}
    </div>
  );
}
