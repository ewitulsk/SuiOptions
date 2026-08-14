// Analytics page (SO-390/SO-397): one dual-axis chart of exchange spot price
// plus annualized realized and implied vol for a catalog instrument, fed by
// the api-service /analytics endpoints (api/analytics.ts).
//
// The three series fetch in parallel and fail independently — each line
// renders without the others, with a per-series status note below the
// chart. A 503 (lake unreachable) shows the friendly "temporarily
// unavailable" copy instead of an error string. IV 400s (no vol index for
// the base) hide the line quietly.

import { useEffect, useRef, useState } from "react";

import {
  AnalyticsUnavailableError,
  useAnalyticsCatalog,
  useIvSeries,
  useRvSeries,
  useSpotSeries,
  type AnalyticsInstrument,
  type AnalyticsSeries,
} from "../api/analytics";
import { AnalyticsChart } from "../components/AnalyticsChart";
import { useSegmentPill } from "../lib/useSegmentPill";

import type { UseQueryResult } from "@tanstack/react-query";

const DEFAULT_INSTRUMENT = "btc-usdc.binance";

// RV defaults per the SO-390 contract; clamped to what the catalog offers.
const DEFAULT_WINDOW_S = 86_400;
const DEFAULT_SAMPLE_INTERVAL_S = 60;
const DEFAULT_ESTIMATOR = "rv_subsampled";

type RangeKey = "7d" | "30d" | "90d" | "1y" | "all";

const RANGES: { key: RangeKey; label: string; days: number | null }[] = [
  { key: "7d", label: "7d", days: 7 },
  { key: "30d", label: "30d", days: 30 },
  { key: "90d", label: "90d", days: 90 },
  { key: "1y", label: "1y", days: 365 },
  { key: "all", label: "All", days: null },
];

/** `YYYY-MM-DD` in UTC, the format /analytics/series takes for from/to. */
function isoDay(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** The user's choice if the catalog offers it, else the contract default,
 * else the first offered value. */
function clamp<T>(offered: T[] | undefined, chosen: T, fallback: T): T {
  if (!offered || offered.length === 0) return chosen;
  if (offered.includes(chosen)) return chosen;
  if (offered.includes(fallback)) return fallback;
  return offered[0];
}

function formatWindow(s: number): string {
  if (s % 86_400 === 0) return `${s / 86_400}d`;
  if (s % 3_600 === 0) return `${s / 3_600}h`;
  if (s % 60 === 0) return `${s / 60}m`;
  return `${s}s`;
}

function RangeFilter({
  range,
  setRange,
}: {
  range: RangeKey;
  setRange: (r: RangeKey) => void;
}) {
  const { ref, geom: pill, animated } = useSegmentPill(range);
  return (
    <div className="act-filters ana-ranges" role="tablist" ref={ref}>
      <span
        className="act-filter__pill"
        aria-hidden
        style={{
          transform: `translate(${pill.left}px, ${pill.top}px)`,
          width: pill.width,
          height: pill.height,
          opacity: pill.ready ? 1 : 0,
          transition: animated ? undefined : "none",
        }}
      />
      {RANGES.map((r) => (
        <button
          key={r.key}
          role="tab"
          aria-selected={range === r.key}
          className={"act-filter" + (range === r.key ? " is-active" : "")}
          onClick={() => setRange(r.key)}
        >
          {r.label}
        </button>
      ))}
    </div>
  );
}

/** "Advanced" popover exposing the three RV params from the catalog. */
function RvAdvanced({
  rv,
  windowS,
  sampleIntervalS,
  estimator,
  onChange,
}: {
  rv: NonNullable<AnalyticsInstrument["metrics"]["rv"]>;
  windowS: number;
  sampleIntervalS: number;
  estimator: string;
  onChange: (p: { windowS?: number; sampleIntervalS?: number; estimator?: string }) => void;
}) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    // pointerdown covers mouse + touch (same pattern as the wallet menu).
    const onClick = (e: PointerEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("pointerdown", onClick);
    return () => window.removeEventListener("pointerdown", onClick);
  }, [open]);

  return (
    <div className="ana-adv" ref={wrapRef}>
      <button
        type="button"
        className={"act-filter" + (open ? " is-open" : "")}
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        Advanced <span aria-hidden>▾</span>
      </button>
      {open && (
        <div className="ana-adv__menu">
          <label className="ana-adv__field">
            <span>RV window</span>
            <select
              className="ana-select"
              value={windowS}
              onChange={(e) => onChange({ windowS: Number(e.target.value) })}
            >
              {rv.windows_s.map((w) => (
                <option key={w} value={w}>
                  {formatWindow(w)}
                </option>
              ))}
            </select>
          </label>
          <label className="ana-adv__field">
            <span>Sample interval</span>
            <select
              className="ana-select"
              value={sampleIntervalS}
              onChange={(e) => onChange({ sampleIntervalS: Number(e.target.value) })}
            >
              {rv.sample_intervals_s.map((s) => (
                <option key={s} value={s}>
                  {formatWindow(s)}
                </option>
              ))}
            </select>
          </label>
          <label className="ana-adv__field">
            <span>Estimator</span>
            <select
              className="ana-select"
              value={estimator}
              onChange={(e) => onChange({ estimator: e.target.value })}
            >
              {rv.estimators.map((est) => (
                <option key={est} value={est}>
                  {est}
                </option>
              ))}
            </select>
          </label>
        </div>
      )}
    </div>
  );
}

/** Per-series status line under the chart; null when the series is healthy. */
function seriesNote(
  label: string,
  q: UseQueryResult<AnalyticsSeries | null, Error>,
): { text: string; kind: "loading" | "unavailable" | "error" | "empty" } | null {
  if (q.isLoading) return { text: `loading ${label}…`, kind: "loading" };
  if (q.error) {
    if (q.error instanceof AnalyticsUnavailableError) {
      return { text: `${label}: analytics temporarily unavailable`, kind: "unavailable" };
    }
    return { text: `${label}: ${q.error.message}`, kind: "error" };
  }
  if (q.data && q.data.points.length === 0) {
    return { text: `${label}: no data for this range`, kind: "empty" };
  }
  return null;
}

export function Analytics() {
  const catalog = useAnalyticsCatalog();
  const instruments = catalog.data?.instruments ?? [];

  const [instrumentId, setInstrumentId] = useState<string | null>(null);
  const [range, setRange] = useState<RangeKey>("30d");
  const [windowS, setWindowS] = useState(DEFAULT_WINDOW_S);
  const [sampleIntervalS, setSampleIntervalS] = useState(DEFAULT_SAMPLE_INTERVAL_S);
  const [estimator, setEstimator] = useState(DEFAULT_ESTIMATOR);

  const inst: AnalyticsInstrument | null =
    instruments.find((i) => i.instrument_id === instrumentId) ??
    instruments.find((i) => i.instrument_id === DEFAULT_INSTRUMENT) ??
    instruments[0] ??
    null;

  // Range → from/to. Relative ranges end tomorrow (UTC) so today's partial
  // data is included; "All" spans the catalog's first/last dates.
  const now = Date.now();
  const rangeDef = RANGES.find((r) => r.key === range)!;
  const from =
    rangeDef.days === null
      ? inst?.first_date ?? isoDay(now)
      : isoDay(now - rangeDef.days * 86_400_000);
  const to =
    rangeDef.days === null ? inst?.last_date ?? isoDay(now) : isoDay(now + 86_400_000);

  // Spot freq auto-selects: 1m for ranges up to 7d, 1h beyond.
  const rangeDays =
    rangeDef.days ??
    (inst
      ? Math.max(1, (Date.parse(inst.last_date) - Date.parse(inst.first_date)) / 86_400_000)
      : 365);
  const freq = clamp(inst?.metrics.spot?.freqs, rangeDays <= 7 ? "1m" : "1h", "1h");

  const rvMeta = inst?.metrics.rv;
  const effWindowS = clamp(rvMeta?.windows_s, windowS, DEFAULT_WINDOW_S);
  const effSampleIntervalS = clamp(
    rvMeta?.sample_intervals_s,
    sampleIntervalS,
    DEFAULT_SAMPLE_INTERVAL_S,
  );
  const effEstimator = clamp(rvMeta?.estimators, estimator, DEFAULT_ESTIMATOR);

  // The three series fetch in parallel; each carries its own error/empty state.
  const spotQ = useSpotSeries(
    inst && inst.metrics.spot
      ? { instrument_id: inst.instrument_id, freq, from, to }
      : null,
  );
  const rvQ = useRvSeries(
    inst && rvMeta
      ? {
          instrument_id: inst.instrument_id,
          window_s: effWindowS,
          sample_interval_s: effSampleIntervalS,
          estimator: effEstimator,
          from,
          to,
        }
      : null,
  );
  // Attempted even when the catalog lacks `metrics.iv` (rollout may lag) —
  // a 400 resolves to `null` and the line hides quietly.
  const ivQ = useIvSeries(inst ? { instrument_id: inst.instrument_id, from, to } : null);

  const spotPoints = spotQ.data?.points ?? [];
  const rvPoints = rvQ.data?.points ?? [];
  const ivPoints = ivQ.data?.points ?? [];
  const loading = spotQ.isLoading || rvQ.isLoading || ivQ.isLoading;
  // iv isn't consulted here: when the overlay shows it's either also 503 or
  // quietly absent (400/empty), and either way the copy below is right.
  const allUnavailable =
    spotQ.error instanceof AnalyticsUnavailableError &&
    rvQ.error instanceof AnalyticsUnavailableError;
  const notes = [
    seriesNote("spot", spotQ),
    seriesNote("realized vol", rvQ),
    seriesNote("implied vol", ivQ),
  ].filter((n): n is NonNullable<typeof n> => n !== null);

  const quoteSymbol = inst?.symbol.split("-")[1] ?? "quote";

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">exchange data · spot, realized & implied vol</div>
          <h1 className="dash-hero__title">Analytics</h1>
          <div className="dash-hero__addr">
            {inst
              ? `${inst.symbol} on ${inst.exchange} · history ${inst.first_date} → ${inst.last_date}`
              : "spot price and annualized realized volatility, from the data lake"}
          </div>
        </div>

        {catalog.isLoading ? (
          <div className="ana-status">loading instrument catalog…</div>
        ) : catalog.error ? (
          <div className="ana-status">
            {catalog.error instanceof AnalyticsUnavailableError
              ? "analytics temporarily unavailable — the data lake is unreachable. Try again in a bit."
              : `couldn't load the instrument catalog: ${catalog.error.message}`}
          </div>
        ) : !inst ? (
          <div className="ana-status">no instruments in the analytics catalog yet.</div>
        ) : (
          <>
            <div className="ana-controls">
              <select
                className="ana-select"
                aria-label="Instrument"
                value={inst.instrument_id}
                onChange={(e) => setInstrumentId(e.target.value)}
              >
                {instruments.map((i) => (
                  <option key={i.instrument_id} value={i.instrument_id}>
                    {i.symbol} · {i.exchange}
                  </option>
                ))}
              </select>
              <RangeFilter range={range} setRange={setRange} />
              {rvMeta && (
                <RvAdvanced
                  rv={rvMeta}
                  windowS={effWindowS}
                  sampleIntervalS={effSampleIntervalS}
                  estimator={effEstimator}
                  onChange={(p) => {
                    if (p.windowS !== undefined) setWindowS(p.windowS);
                    if (p.sampleIntervalS !== undefined) setSampleIntervalS(p.sampleIntervalS);
                    if (p.estimator !== undefined) setEstimator(p.estimator);
                  }}
                />
              )}
            </div>

            <div className="panel">
              <div className="panel__head">
                {inst.symbol} · spot ({quoteSymbol}) vs realized (
                {formatWindow(effWindowS)} window) & implied vol, annualized
              </div>
              <div className="ana-chart__holder">
                <AnalyticsChart
                  spot={spotPoints}
                  rv={rvPoints}
                  iv={ivPoints}
                  quoteSymbol={quoteSymbol}
                />
                {!loading &&
                  spotPoints.length === 0 &&
                  rvPoints.length === 0 &&
                  ivPoints.length === 0 && (
                    <div className="ana-chart__empty">
                      <div className="ana-chart__empty-title">
                        {allUnavailable
                          ? "analytics temporarily unavailable"
                          : "no data for this range"}
                      </div>
                      <div className="ana-chart__empty-sub">
                        {allUnavailable
                          ? "The data lake is unreachable right now — try again in a bit."
                          : "Try a wider range or another instrument."}
                      </div>
                    </div>
                  )}
              </div>
              {notes.length > 0 && (
                <div className="panel__sub">
                  {notes.map((n) => (
                    <div key={n.text}>{n.text}</div>
                  ))}
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
