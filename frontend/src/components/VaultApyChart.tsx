// APY-over-time chart for a vault.
//
// Two series share the panel (TradingView Lightweight Charts v5, themed from
// CSS vars, like `ChartPanel`): a SOLID line for realized APY (annualized pps
// growth per finalized round, from the indexer) and a DASHED line for
// predicted APY (forward premium-yield, from derived-metric-worker). The
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
import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
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

    chartRef.current = chart;
    realizedRef.current = realizedLine;
    predictedRef.current = predictedLine;
    return () => {
      chart.remove();
      chartRef.current = null;
      realizedRef.current = null;
      predictedRef.current = null;
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
  }, [realized, predicted]);

  const empty = realized.length === 0 && predicted.length === 0;

  return (
    <div className="vault-card vault-chart">
      <div
        className="vault-card__head"
        style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}
      >
        <span>
          <span className="panel__head-dot" />
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
        Projected (dashed) is a premium-yield estimate, not a guarantee — it
        assumes calls expire unassigned; realized may come in lower in a rally.
      </div>
    </div>
  );
}
