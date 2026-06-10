// Candlestick + volume chart for a bucket's DeepBook pool (SO-157).
//
// TradingView Lightweight Charts (v5, open source) fed by the
// price-charting service: REST history + live WS bar updates via `useBars`.
// A horizontal price line marks the bucket's strike for context.

import { useEffect, useRef, useState } from "react";
import {
  CandlestickSeries,
  ColorType,
  HistogramSeries,
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

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function ChartPanel({ poolId, strike, settlementSymbol }: Props) {
  const [interval, setInterval] = useState<ChartInterval>("5m");
  const { bars, loading } = useBars(poolId, interval);
  const mode = useThemeMode();

  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const candlesRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);

  // (Re)create the chart when the container mounts or the theme flips —
  // Lightweight Charts reads colors once, so a theme change needs a rebuild.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;

    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const up = cssVar("--aqua-up", "#1fbf75");
    const down = cssVar("--aqua-down", "#e15d6b");

    const chart = createChart(el, {
      height: 280,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: ink,
        fontFamily: "inherit",
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

    if (strike !== null && Number.isFinite(strike)) {
      candles.createPriceLine({
        price: strike,
        color: ink,
        lineStyle: 2, // dashed
        lineWidth: 1,
        title: "strike",
      });
    }

    chartRef.current = chart;
    candlesRef.current = candles;
    volumeRef.current = volume;
    return () => {
      chart.remove();
      chartRef.current = null;
      candlesRef.current = null;
      volumeRef.current = null;
    };
    // strike is constant per bucket; mode rebuilds for theme colors.
  }, [mode, strike, poolId]);

  // Feed data. 300 bars — full setData on change is plenty fast and keeps
  // the merge logic in one place (useBars).
  useEffect(() => {
    const candles = candlesRef.current;
    const volume = volumeRef.current;
    if (!candles || !volume) return;
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
  }, [bars]);

  return (
    <div className="panel" style={{ marginTop: 12 }}>
      <div
        className="panel__head"
        style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}
      >
        <span>
          <span className="panel__head-dot"></span>market · {settlementSymbol} per option
        </span>
        <span style={{ display: "flex", gap: 4 }}>
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
      {!loading && bars.length === 0 && (
        <div className="panel__sub" style={{ marginTop: 6 }}>
          No trades yet — the chart fills in as the market trades.
        </div>
      )}
    </div>
  );
}
