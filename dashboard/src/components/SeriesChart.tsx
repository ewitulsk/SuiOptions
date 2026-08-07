// Generic multi-line time-series chart (Lightweight Charts v5), themed
// from the dashboard CSS vars. Rebuilt on theme flip.

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
  createChart,
  type IChartApi,
  type UTCTimestamp,
} from "lightweight-charts";

import { useThemeMode } from "../theme";

function cssVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export type ChartLine = {
  name: string;
  color?: string;
  points: Array<{ timeMs: number; value: number }>;
};

const PALETTE = ["#4da2ff", "#1e8694", "#ff7a6e", "#d9930d", "#22b07a", "#9d7bff"];

export function SeriesChart(props: { lines: ChartLine[]; height?: number; digits?: number }) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const height = props.height ?? 220;
  const digits = props.digits ?? 2;

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
    for (const [i, line] of props.lines.entries()) {
      const s = chart.addSeries(LineSeries, {
        color: line.color ?? PALETTE[i % PALETTE.length],
        lineWidth: 2,
        priceLineVisible: false,
        title: line.name,
        priceFormat: { type: "custom", formatter: (v: number) => v.toFixed(digits) },
      });
      const bySecond = new Map<number, number>();
      for (const p of [...line.points].sort((a, b) => a.timeMs - b.timeMs)) {
        if (Number.isFinite(p.value)) bySecond.set(Math.floor(p.timeMs / 1000), p.value);
      }
      s.setData(
        [...bySecond.entries()].map(([time, value]) => ({ time: time as UTCTimestamp, value })),
      );
    }
    chart.timeScale().fitContent();
    chartRef.current = chart;
    return () => {
      chart.remove();
      chartRef.current = null;
    };
    // Rebuild whenever the data or theme changes — series count may differ.
  }, [mode, height, digits, props.lines]);

  return <div ref={holderRef} style={{ width: "100%", height }} />;
}
