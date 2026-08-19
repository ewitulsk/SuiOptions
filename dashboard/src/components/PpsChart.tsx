// Share-price line chart — same Lightweight Charts v5 setup as the
// frontend's TradingVaultPpsChart, themed from the dashboard CSS vars.
//
// v2 (SO-418): the pps-history feed is per-tranche. Tranched vaults draw
// senior + junior as two series, with markers on junior generation resets
// (junior pps re-bases to 1.0 — without a marker this looks like a
// rendering bug); untranched vaults draw the single "untranched" series.

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  createChart,
  createSeriesMarkers,
  type IChartApi,
  type ISeriesApi,
  type SeriesMarker,
  type UTCTimestamp,
} from "lightweight-charts";

import type { PpsPoint, TrancheLabel } from "../api/vault";
import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

/** Strictly increasing unique times, as the library requires: collapse
 * same-second samples (keep the latest) and sort ascending. */
function toLineData(points: PpsPoint[]): { time: UTCTimestamp; value: number }[] {
  const bySecond = new Map<number, number>();
  for (const p of [...points].sort((a, b) => a.timestampMs - b.timestampMs)) {
    bySecond.set(Math.floor(p.timestampMs / 1000), p.pps);
  }
  return [...bySecond.entries()].map(([time, value]) => ({ time: time as UTCTimestamp, value }));
}

export function PpsChart(props: { points: PpsPoint[]; tranched: boolean; height?: number }) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<Map<TrancheLabel, ISeriesApi<"Line">> | null>(null);
  const height = props.height ?? 200;
  const tranched = props.tranched;

  // Rebuild on theme flip (the chart reads colors once) and when the
  // vault's structure resolves — the series set differs.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const chart = createChart(el, {
      height,
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: cssVar("--aqua-ink-2", "#5c6b7a"),
        fontFamily: "inherit",
        attributionLogo: false,
      },
      grid: {
        vertLines: { color: cssVar("--aqua-line", "rgba(92,107,122,0.12)") },
        horzLines: { color: cssVar("--aqua-line", "rgba(92,107,122,0.12)") },
      },
      handleScroll: { vertTouchDrag: false },
      rightPriceScale: { borderVisible: false },
      timeScale: { borderVisible: false, timeVisible: true, secondsVisible: false },
    });
    const mkLine = (color: string) =>
      chart.addSeries(LineSeries, {
        color,
        lineWidth: 2,
        priceLineVisible: false,
        priceFormat: { type: "custom", formatter: (v: number) => v.toFixed(4) },
      });
    const series = new Map<TrancheLabel, ISeriesApi<"Line">>();
    if (tranched) {
      series.set("senior", mkLine(cssVar("--aqua-success", "#1fbf75")));
      series.set("junior", mkLine(cssVar("--aqua-sui", "#4da2ff")));
    } else {
      series.set("untranched", mkLine(cssVar("--aqua-sui", "#4da2ff")));
    }
    chartRef.current = chart;
    seriesRef.current = series;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, [mode, height, tranched]);

  useEffect(() => {
    const series = seriesRef.current;
    if (!series) return;
    for (const [tranche, line] of series) {
      const mine = props.points.filter((p) => p.tranche === tranche);
      line.setData(toLineData(mine));
      // Junior generation-reset markers: the pps re-base is deliberate.
      if (tranche === "junior") {
        const markers: SeriesMarker<UTCTimestamp>[] = mine
          .filter((p) => p.reset)
          .sort((a, b) => a.timestampMs - b.timestampMs)
          .map((p) => ({
            time: Math.floor(p.timestampMs / 1000) as UTCTimestamp,
            position: "aboveBar",
            color: cssVar("--aqua-coral", "#e05555"),
            shape: "arrowDown",
            text: "reset",
          }));
        createSeriesMarkers(line, markers);
      }
    }
    chartRef.current?.timeScale().fitContent();
  }, [props.points, mode, tranched]);

  return (
    <div>
      {tranched && (
        <div style={{ fontSize: 11, marginBottom: 4 }}>
          <span style={{ color: "var(--aqua-success, #1fbf75)" }}>— senior</span>
          <span style={{ color: "var(--aqua-sui, #4da2ff)", marginLeft: 8 }}>— junior</span>
        </div>
      )}
      <div ref={holderRef} style={{ width: "100%", height }} />
    </div>
  );
}
