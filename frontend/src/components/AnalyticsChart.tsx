// Dual-axis analytics chart (SO-390/SO-397): spot price (left axis, quote
// units) plus annualized realized and implied vol (shared right axis,
// percent) on one time scale.
//
// Same TradingView Lightweight Charts v5 setup as `ChartPanel` /
// `TradingVaultPpsChart` — themed from CSS vars, rebuilt on theme flip.
// The legend doubles as the hover tooltip: crosshair moves update the
// readout for both series; off-chart it shows the latest values.

import { useEffect, useRef, useState } from "react";
import {
  ColorType,
  LineSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type MouseEventParams,
  type UTCTimestamp,
} from "lightweight-charts";

import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

/** Collapse `[unix_ms, value]` pairs to strictly-increasing unique seconds
 * (keep the latest per second), as the chart library requires. */
function toLineData(points: [number, number][]) {
  const bySecond = new Map<number, number>();
  for (const [t, v] of [...points].sort((a, b) => a[0] - b[0])) {
    bySecond.set(Math.floor(t / 1000), v);
  }
  return [...bySecond.entries()].map(([time, value]) => ({
    time: time as UTCTimestamp,
    value,
  }));
}

type Hover = {
  time: number | null;
  spot: number | null;
  rv: number | null;
  iv: number | null;
};

type Props = {
  /** `[unix_ms, quote_price]` pairs. */
  spot: [number, number][];
  /** `[unix_ms, annualized_vol_fraction]` pairs — plotted as `value * 100`%. */
  rv: [number, number][];
  /** `[unix_ms, annualized_vol_fraction]` pairs (vol index, e.g. DVOL) —
   * same right percent axis as `rv`. */
  iv: [number, number][];
  /** Quote-unit label for the spot legend readout, e.g. `"USDC"`. */
  quoteSymbol: string;
};

export function AnalyticsChart({ spot, rv, iv, quoteSymbol }: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const spotRef = useRef<ISeriesApi<"Line"> | null>(null);
  const rvRef = useRef<ISeriesApi<"Line"> | null>(null);
  const ivRef = useRef<ISeriesApi<"Line"> | null>(null);
  const [hover, setHover] = useState<Hover>({ time: null, spot: null, rv: null, iv: null });

  // Rebuild on theme flip — Lightweight Charts reads colors once.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const accent = cssVar("--aqua-accent", "#2f81f7");
    const up = cssVar("--aqua-up", "#1fbf75");
    const coral = cssVar("--aqua-coral", "#FF7A6E");

    const chart = createChart(el, {
      height: 320,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: ink,
        fontFamily: "inherit",
        attributionLogo: false,
      },
      grid: { vertLines: { color: grid }, horzLines: { color: grid } },
      // A vertical touch drag scrolls the page, not the chart's price scale.
      handleScroll: { vertTouchDrag: false },
      leftPriceScale: { visible: true, borderVisible: false },
      rightPriceScale: { visible: true, borderVisible: false },
      timeScale: { borderVisible: false, timeVisible: true, secondsVisible: false },
    });
    const spotLine = chart.addSeries(LineSeries, {
      priceScaleId: "left",
      color: accent,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
    });
    const percentFormat = {
      type: "custom" as const,
      formatter: (v: number) => `${(v * 100).toFixed(1)}%`,
    };
    const rvLine = chart.addSeries(LineSeries, {
      priceScaleId: "right",
      color: up,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: percentFormat,
    });
    const ivLine = chart.addSeries(LineSeries, {
      priceScaleId: "right",
      color: coral,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: percentFormat,
    });

    const onCrosshair = (param: MouseEventParams) => {
      if (param.time === undefined) {
        setHover({ time: null, spot: null, rv: null, iv: null });
        return;
      }
      const s = param.seriesData.get(spotLine) as { value?: number } | undefined;
      const r = param.seriesData.get(rvLine) as { value?: number } | undefined;
      const i = param.seriesData.get(ivLine) as { value?: number } | undefined;
      setHover({
        time: (param.time as number) * 1000,
        spot: s?.value ?? null,
        rv: r?.value ?? null,
        iv: i?.value ?? null,
      });
    };
    chart.subscribeCrosshairMove(onCrosshair);

    chartRef.current = chart;
    spotRef.current = spotLine;
    rvRef.current = rvLine;
    ivRef.current = ivLine;
    return () => {
      chart.unsubscribeCrosshairMove(onCrosshair);
      chart.remove();
      chartRef.current = null;
      spotRef.current = null;
      rvRef.current = null;
      ivRef.current = null;
    };
  }, [mode]);

  useEffect(() => {
    const spotLine = spotRef.current;
    const rvLine = rvRef.current;
    const ivLine = ivRef.current;
    if (!spotLine || !rvLine || !ivLine) return;
    spotLine.setData(toLineData(spot));
    rvLine.setData(toLineData(rv));
    ivLine.setData(toLineData(iv));
    chartRef.current?.timeScale().fitContent();
  }, [spot, rv, iv, mode]);

  // Off-chart, the legend reads the latest point of each series.
  const lastSpot = spot.length > 0 ? spot[spot.length - 1][1] : null;
  const lastRv = rv.length > 0 ? rv[rv.length - 1][1] : null;
  const lastIv = iv.length > 0 ? iv[iv.length - 1][1] : null;
  const shownSpot = hover.time !== null ? hover.spot : lastSpot;
  const shownRv = hover.time !== null ? hover.rv : lastRv;
  const shownIv = hover.time !== null ? hover.iv : lastIv;

  return (
    <div>
      <div className="ana-legend">
        <span className="ana-legend__item">
          <span className="ana-legend__swatch ana-legend__swatch--spot" aria-hidden />
          Spot
          <span className="ana-legend__val">
            {shownSpot !== null && Number.isFinite(shownSpot)
              ? `${shownSpot.toLocaleString("en-US", { maximumFractionDigits: 2 })} ${quoteSymbol}`
              : "—"}
          </span>
        </span>
        <span className="ana-legend__item">
          <span className="ana-legend__swatch ana-legend__swatch--rv" aria-hidden />
          Realized vol
          <span className="ana-legend__val">
            {shownRv !== null && Number.isFinite(shownRv)
              ? `${(shownRv * 100).toFixed(1)}%`
              : "—"}
          </span>
        </span>
        <span className="ana-legend__item">
          <span className="ana-legend__swatch ana-legend__swatch--iv" aria-hidden />
          Implied vol (DVOL)
          <span className="ana-legend__val">
            {shownIv !== null && Number.isFinite(shownIv)
              ? `${(shownIv * 100).toFixed(1)}%`
              : "—"}
          </span>
        </span>
        {hover.time !== null && (
          <span className="ana-legend__time">
            {new Date(hover.time).toLocaleString("en-US", {
              month: "short",
              day: "numeric",
              hour: "numeric",
              minute: "2-digit",
            })}
          </span>
        )}
      </div>
      <div ref={holderRef} style={{ width: "100%", height: 320 }} />
    </div>
  );
}
