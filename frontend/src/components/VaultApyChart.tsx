// APY-over-time chart for a vault.
//
// Wired exactly like `ChartPanel` (TradingView Lightweight Charts v5, themed
// from CSS vars) but fed by `useVaultApyHistory`, which has NO backing endpoint
// yet (see its doc comment). Until that endpoint ships the series is empty and
// the panel renders a "coming soon" placeholder — leaving the slot in the page
// so the only change required later is the hook's `queryFn`.

import { useEffect, useRef } from "react";
import {
  ColorType,
  LineSeries,
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
  points: VaultApyPoint[];
  loading: boolean;
};

export function VaultApyChart({ points, loading }: Props) {
  const mode = useThemeMode();
  const holderRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const lineRef = useRef<ISeriesApi<"Line"> | null>(null);

  // Rebuild on theme flip — Lightweight Charts reads colors once.
  useEffect(() => {
    const el = holderRef.current;
    if (!el) return;
    const ink = cssVar("--aqua-ink-2", "#5c6b7a");
    const grid = cssVar("--aqua-line", "rgba(92,107,122,0.12)");
    const accent = cssVar("--aqua-up", "#1fbf75");

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
    const line = chart.addSeries(LineSeries, {
      color: accent,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
      // Render the value as a percentage (apy is a fraction, e.g. 0.11 → 11%).
      priceFormat: { type: "custom", formatter: (v: number) => `${(v * 100).toFixed(1)}%` },
    });

    chartRef.current = chart;
    lineRef.current = line;
    return () => {
      chart.remove();
      chartRef.current = null;
      lineRef.current = null;
    };
  }, [mode]);

  useEffect(() => {
    const line = lineRef.current;
    if (!line) return;
    line.setData(
      points.map((p) => ({ time: (p.t_ms / 1000) as UTCTimestamp, value: p.apy })),
    );
  }, [points]);

  const empty = points.length === 0;

  return (
    <div className="vault-card vault-chart">
      <div className="vault-card__head">
        <span className="panel__head-dot" />
        APY over time
      </div>
      <div className="vault-chart__holder">
        <div ref={holderRef} style={{ width: "100%", height: 260 }} />
        {!loading && empty && (
          <div className="vault-chart__empty">
            <div className="vault-chart__empty-title">APY history coming soon</div>
            <div className="vault-chart__empty-sub">
              The strategy publishes a realized APY each time a round finalizes.
              Once a few rounds settle, the curve fills in here.
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
