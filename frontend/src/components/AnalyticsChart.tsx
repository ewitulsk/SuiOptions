// Dual-axis analytics chart (SO-390/SO-397): spot price (left axis, quote
// units) plus annualized realized/implied vol and perp funding (shared right
// axis, percent) on one time scale. Perp basis is ~100x smaller than vol, so
// it rides its own overlay price scale instead of the shared percent axis.
//
// Same TradingView Lightweight Charts v5 setup as `ChartPanel` /
// `TradingVaultPpsChart` — themed from CSS vars, rebuilt on theme flip.
// The legend doubles as the hover tooltip AND the visibility toggles:
// clicking an entry shows/hides its line; crosshair moves update the
// readout for visible series; off-chart it shows the latest values.

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

export type AnalyticsSeriesKey = "spot" | "rv" | "iv" | "funding" | "basis";

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
  funding: number | null;
  basis: number | null;
};

const NO_HOVER: Hover = { time: null, spot: null, rv: null, iv: null, funding: null, basis: null };

type Props = {
  /** `[unix_ms, quote_price]` pairs. */
  spot: [number, number][];
  /** `[unix_ms, annualized_vol_fraction]` pairs — plotted as `value * 100`%. */
  rv: [number, number][];
  /** `[unix_ms, annualized_vol_fraction]` pairs (vol index, e.g. DVOL) —
   * same right percent axis as `rv`. */
  iv: [number, number][];
  /** `[unix_ms, annualized_rate_fraction]` pairs (perps only, can be
   * negative) — same right percent axis as `rv`/`iv`. */
  funding: [number, number][];
  /** `[unix_ms, premium_fraction]` pairs (perps only, ~±0.3%) — own overlay
   * price scale, NOT the shared percent axis. */
  basis: [number, number][];
  /** Offer the funding/basis legend entries (perp instruments only). */
  perp: boolean;
  visible: Record<AnalyticsSeriesKey, boolean>;
  onToggleSeries: (key: AnalyticsSeriesKey) => void;
  /** Quote-unit label for the spot legend readout, e.g. `"USDC"`. */
  quoteSymbol: string;
};

export function AnalyticsChart({
  spot,
  rv,
  iv,
  funding,
  basis,
  perp,
  visible,
  onToggleSeries,
  quoteSymbol,
}: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRefs = useRef<Record<AnalyticsSeriesKey, ISeriesApi<"Line"> | null>>({
    spot: null,
    rv: null,
    iv: null,
    funding: null,
    basis: null,
  });
  const [hover, setHover] = useState<Hover>(NO_HOVER);

  // Rebuild on theme flip — Lightweight Charts reads colors once.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const accent = cssVar("--aqua-accent", "#2f81f7");
    const up = cssVar("--aqua-up", "#1fbf75");
    const coral = cssVar("--aqua-coral", "#FF7A6E");
    const fundingColor = cssVar("--aqua-funding", "#1E8694");
    const basisColor = cssVar("--aqua-basis", "#8E6BD8");

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
    // Funding shares the right percent axis — annualized magnitudes are
    // comparable with vol, and the shared autoscale absorbs negative values.
    const fundingLine = chart.addSeries(LineSeries, {
      priceScaleId: "right",
      color: fundingColor,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: percentFormat,
    });
    // Basis is ~100x smaller than vol, so it gets its own overlay scale
    // (invisible axis) squeezed into the lower band of the pane.
    const basisLine = chart.addSeries(LineSeries, {
      priceScaleId: "basis",
      color: basisColor,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      priceFormat: {
        type: "custom" as const,
        formatter: (v: number) => `${(v * 100).toFixed(3)}%`,
      },
    });
    basisLine.priceScale().applyOptions({ scaleMargins: { top: 0.65, bottom: 0.05 } });

    const onCrosshair = (param: MouseEventParams) => {
      if (param.time === undefined) {
        setHover(NO_HOVER);
        return;
      }
      const at = (line: ISeriesApi<"Line">) =>
        (param.seriesData.get(line) as { value?: number } | undefined)?.value ?? null;
      setHover({
        time: (param.time as number) * 1000,
        spot: at(spotLine),
        rv: at(rvLine),
        iv: at(ivLine),
        funding: at(fundingLine),
        basis: at(basisLine),
      });
    };
    chart.subscribeCrosshairMove(onCrosshair);

    chartRef.current = chart;
    seriesRefs.current = {
      spot: spotLine,
      rv: rvLine,
      iv: ivLine,
      funding: fundingLine,
      basis: basisLine,
    };
    return () => {
      chart.unsubscribeCrosshairMove(onCrosshair);
      chart.remove();
      chartRef.current = null;
      seriesRefs.current = { spot: null, rv: null, iv: null, funding: null, basis: null };
    };
  }, [mode]);

  useEffect(() => {
    const s = seriesRefs.current;
    if (!s.spot || !s.rv || !s.iv || !s.funding || !s.basis) return;
    s.spot.setData(toLineData(spot));
    s.rv.setData(toLineData(rv));
    s.iv.setData(toLineData(iv));
    s.funding.setData(toLineData(funding));
    s.basis.setData(toLineData(basis));
    chartRef.current?.timeScale().fitContent();
  }, [spot, rv, iv, funding, basis, mode]);

  // Legend toggles map straight onto series visibility.
  useEffect(() => {
    const s = seriesRefs.current;
    s.spot?.applyOptions({ visible: visible.spot });
    s.rv?.applyOptions({ visible: visible.rv });
    s.iv?.applyOptions({ visible: visible.iv });
    s.funding?.applyOptions({ visible: perp && visible.funding });
    s.basis?.applyOptions({ visible: perp && visible.basis });
  }, [visible, perp, mode]);

  // Off-chart, the legend reads the latest point of each series.
  const last = (pts: [number, number][]) => (pts.length > 0 ? pts[pts.length - 1][1] : null);
  const shown = (key: AnalyticsSeriesKey, pts: [number, number][]) =>
    hover.time !== null ? hover[key] : last(pts);

  const fmtPct = (v: number, decimals: number) => `${(v * 100).toFixed(decimals)}%`;

  const legendItem = (
    key: AnalyticsSeriesKey,
    label: string,
    pts: [number, number][],
    fmt: (v: number) => string,
  ) => {
    const on = visible[key];
    const v = shown(key, pts);
    return (
      <button
        type="button"
        className={"ana-legend__item" + (on ? "" : " is-off")}
        aria-pressed={on}
        title={on ? `Hide ${label}` : `Show ${label}`}
        onClick={() => onToggleSeries(key)}
      >
        <span className={`ana-legend__swatch ana-legend__swatch--${key}`} aria-hidden />
        {label}
        {on && (
          <span className="ana-legend__val">
            {v !== null && Number.isFinite(v) ? fmt(v) : "—"}
          </span>
        )}
      </button>
    );
  };

  return (
    <div>
      <div className="ana-legend">
        {legendItem(
          "spot",
          "Spot",
          spot,
          (v) => `${v.toLocaleString("en-US", { maximumFractionDigits: 2 })} ${quoteSymbol}`,
        )}
        {legendItem("rv", "Realized vol", rv, (v) => fmtPct(v, 1))}
        {legendItem("iv", "Implied vol (DVOL)", iv, (v) => fmtPct(v, 1))}
        {perp && legendItem("funding", "Funding (ann.)", funding, (v) => fmtPct(v, 1))}
        {perp && legendItem("basis", "Basis", basis, (v) => fmtPct(v, 3))}
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
