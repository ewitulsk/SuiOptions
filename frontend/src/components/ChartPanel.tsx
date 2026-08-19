// Price chart for a bucket's exchange market (SO-157, SO-416 — the market's
// registry id is passed as `poolId`; price-charting keys exchange markets
// under it).
//
// TradingView Lightweight Charts (v5, open source) fed by the
// price-charting service: REST history + live WS updates via `useBars`.
// Two switchable series share the panel: the order-book midpoint line
// (default — moves whenever MMs re-quote) and trade candles + volume
// (only moves on fills). A horizontal price line marks the bucket's
// strike for context.

import { useEffect, useRef, useState } from "react";
import {
  CandlestickSeries,
  ColorType,
  HistogramSeries,
  LineSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";

import { CHART_INTERVALS, useBars, type ChartInterval } from "../api/charts";
import { useThemeMode } from "../theme";

type Props = {
  poolId: string;
  /** Bucket strike in quote units, for the reference line. `null` hides it. */
  strike: number | null;
  settlementSymbol: string;
};

type SeriesMode = "mid" | "trades";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function ChartPanel({ poolId, strike, settlementSymbol }: Props) {
  const [interval, setInterval] = useState<ChartInterval>("5m");
  const [seriesMode, setSeriesMode] = useState<SeriesMode>("mid");
  const { bars, mids, loading } = useBars(poolId, interval);
  const mode = useThemeMode();

  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const candlesRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const midLineRef = useRef<ISeriesApi<"Line"> | null>(null);

  // (Re)create the chart when the container mounts or the theme flips —
  // Lightweight Charts reads colors once, so a theme change needs a rebuild.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;

    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const up = cssVar("--aqua-up", "#1fbf75");
    const down = cssVar("--aqua-down", "#e15d6b");
    const accent = cssVar("--aqua-accent", "#2f81f7");

    const chart = createChart(el, {
      height: 280,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: ink,
        fontFamily: "inherit",
        attributionLogo: false,
      },
      grid: {
        vertLines: { color: grid },
        horzLines: { color: grid },
      },
      rightPriceScale: { borderVisible: false },
      timeScale: { borderVisible: false, timeVisible: true, secondsVisible: false },
    });
    const candles = chart.addSeries(CandlestickSeries, {
      upColor: up,
      downColor: down,
      borderVisible: false,
      wickUpColor: up,
      wickDownColor: down,
    });
    const volume = chart.addSeries(HistogramSeries, {
      priceFormat: { type: "volume" },
      priceScaleId: "vol",
      color: grid,
    });
    chart.priceScale("vol").applyOptions({ scaleMargins: { top: 0.82, bottom: 0 } });
    const midLine = chart.addSeries(LineSeries, {
      color: accent,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
    });

    if (strike !== null && Number.isFinite(strike)) {
      // Attach to both price-bearing series — only the populated one renders.
      for (const s of [candles, midLine]) {
        s.createPriceLine({
          price: strike,
          color: ink,
          lineStyle: 2, // dashed
          lineWidth: 1,
          title: "strike",
        });
      }
    }

    chartRef.current = chart;
    candlesRef.current = candles;
    volumeRef.current = volume;
    midLineRef.current = midLine;
    return () => {
      chart.remove();
      chartRef.current = null;
      candlesRef.current = null;
      volumeRef.current = null;
      midLineRef.current = null;
    };
    // strike is constant per bucket; mode rebuilds for theme colors.
  }, [mode, strike, poolId]);

  // Feed data. 300 bars — full setData on change is plenty fast and keeps
  // the merge logic in one place (useBars). The inactive series is cleared
  // rather than removed so toggling keeps zoom/scroll state.
  useEffect(() => {
    const candles = candlesRef.current;
    const volume = volumeRef.current;
    const midLine = midLineRef.current;
    if (!candles || !volume || !midLine) return;
    if (seriesMode === "trades") {
      candles.setData(
        bars.map((b) => ({
          time: (b.t / 1000) as UTCTimestamp,
          open: b.o,
          high: b.h,
          low: b.l,
          close: b.c,
        })),
      );
      volume.setData(
        bars.map((b) => ({ time: (b.t / 1000) as UTCTimestamp, value: b.v })),
      );
      midLine.setData([]);
    } else {
      midLine.setData(
        mids.map((p) => ({ time: (p.t / 1000) as UTCTimestamp, value: p.m })),
      );
      candles.setData([]);
      volume.setData([]);
    }
  }, [bars, mids, seriesMode]);

  const empty = seriesMode === "trades" ? bars.length === 0 : mids.length === 0;

  return (
    <div className="panel">
      <div
        className="panel__head"
        style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}
      >
        <span>
          market · {settlementSymbol} per option
        </span>
        <span style={{ display: "flex", gap: 4, alignItems: "center" }}>
          {(["mid", "trades"] as SeriesMode[]).map((sm) => (
            <button
              key={sm}
              onClick={() => setSeriesMode(sm)}
              style={{
                border: "none",
                background: sm === seriesMode ? "var(--aqua-line, rgba(92,107,122,0.18))" : "transparent",
                color: "inherit",
                borderRadius: 6,
                padding: "2px 8px",
                cursor: "pointer",
                fontSize: 11,
              }}
            >
              {sm}
            </button>
          ))}
          <span style={{ width: 8 }} />
          {CHART_INTERVALS.map((iv) => (
            <button
              key={iv}
              onClick={() => setInterval(iv)}
              style={{
                border: "none",
                background: iv === interval ? "var(--aqua-line, rgba(92,107,122,0.18))" : "transparent",
                color: "inherit",
                borderRadius: 6,
                padding: "2px 8px",
                cursor: "pointer",
                fontSize: 11,
              }}
            >
              {iv}
            </button>
          ))}
        </span>
      </div>
      <div ref={holderRef} style={{ width: "100%", height: 280 }} />
      {!loading && empty && (
        <div className="panel__sub" style={{ marginTop: 6 }}>
          {seriesMode === "trades"
            ? "No trades yet — the chart fills in as the market trades."
            : "No quotes yet — the mid line fills in as market makers quote the book."}
        </div>
      )}
    </div>
  );
}
