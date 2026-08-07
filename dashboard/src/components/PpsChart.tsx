// Share-price line chart — same Lightweight Charts v5 setup as the
// frontend's TradingVaultPpsChart, themed from the dashboard CSS vars.

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";

import { PPS_E12, type PpsPoint } from "../api/vault";
import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function PpsChart(props: { points: PpsPoint[]; height?: number }) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const height = props.height ?? 200;

  // Rebuild on theme flip — the chart reads colors once.
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
    const line = chart.addSeries(LineSeries, {
      color: cssVar("--aqua-sui", "#4da2ff"),
      lineWidth: 2,
      priceLineVisible: false,
      priceFormat: { type: "custom", formatter: (v: number) => v.toFixed(4) },
    });
    chartRef.current = chart;
    seriesRef.current = line;
    return () => {
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, [mode, height]);

  useEffect(() => {
    const line = seriesRef.current;
    if (!line) return;
    // Strictly increasing unique times, as the library requires.
    const bySecond = new Map<number, number>();
    for (const p of [...props.points].sort(
      (a, b) => Number(a.timestampMs) - Number(b.timestampMs),
    )) {
      bySecond.set(Math.floor(Number(p.timestampMs) / 1000), Number(p.ppsE12) / PPS_E12);
    }
    line.setData(
      [...bySecond.entries()].map(([time, value]) => ({ time: time as UTCTimestamp, value })),
    );
    chartRef.current?.timeScale().fitContent();
  }, [props.points, mode]);

  return <div ref={holderRef} style={{ width: "100%", height }} />;
}
