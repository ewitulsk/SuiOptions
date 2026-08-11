// Share-price-over-time chart for a curated trading vault (SO-293).
//
// One solid line of price-per-share in deposit-asset units, from the
// api-service pps-history endpoint. Same TradingView Lightweight Charts v5
// setup as `VaultApyChart` (themed from CSS vars, rebuilt on theme flip).

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";

import { PPS_E12, type TradingVaultPpsPoint } from "../api/tradingVaults";
import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

type Props = {
  points: TradingVaultPpsPoint[];
  loading: boolean;
  /** Deposit-asset ticker, for the axis/legend label. */
  symbol: string;
};

export function TradingVaultPpsChart({ points, loading, symbol }: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Line"> | null>(null);

  // Rebuild on theme flip — Lightweight Charts reads colors once.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const accent = cssVar("--aqua-accent", "#2f81f7");

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
    const line = chart.addSeries(LineSeries, {
      color: accent,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: {
        type: "custom",
        formatter: (v: number) => v.toFixed(4),
      },
    });

    chartRef.current = chart;
    seriesRef.current = line;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, [mode]);

  useEffect(() => {
    const line = seriesRef.current;
    if (!line) return;
    // Below two points the empty-state overlay is showing — plot nothing, or
    // a dead one-point line (and its axis price tag) draws through the text.
    const usable = points.length < 2 ? [] : points;
    // Strictly increasing unique times: collapse same-second samples (keep
    // the latest) and sort ascending, as the chart library requires.
    const bySecond = new Map<number, number>();
    for (const p of [...usable].sort((a, b) => Number(a.timestampMs) - Number(b.timestampMs))) {
      // The SO-370 virtual offset in raw pps cancels against the offset a
      // display share carries — pps_e12 / 1e12 is the display share price
      // (matches `tradingVaultPps`).
      bySecond.set(
        Math.floor(Number(p.timestampMs) / 1000),
        Number(p.ppsE12) / PPS_E12,
      );
    }
    line.setData(
      [...bySecond.entries()].map(([time, value]) => ({ time: time as UTCTimestamp, value })),
    );
    chartRef.current?.timeScale().fitContent();
  }, [points, mode]);

  const empty = points.length < 2;

  return (
    <div className="vault-card vault-chart">
      <div className="vault-card__head">Share price over time</div>
      <div className="vault-chart__holder">
        <div ref={holderRef} style={{ width: "100%", height: 220 }} />
        {!loading && empty && (
          <div className="vault-chart__empty">
            <div className="vault-chart__empty-title">No share-price history yet</div>
            <div className="vault-chart__empty-sub">
              The vault records a share price at each appraisal — deposits,
              withdrawal fulfillments, and curator marks. The curve fills in
              here as the vault operates.
            </div>
          </div>
        )}
      </div>
      <div className="vault-card__foot vault-prose__muted">
        Price per share in {symbol}, from on-chain appraisals.
      </div>
    </div>
  );
}
