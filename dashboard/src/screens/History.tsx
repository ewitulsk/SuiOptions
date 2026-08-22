// History — TimescaleDB-backed time series from mm-bot /desk/history.

import { useMemo, useState } from "react";

import {
  useDeskHistory,
  type PnlPoint,
  type SnapshotPoint,
  type SymbolPoint,
  type VenuePoint,
} from "../api/deskHistory";
import { enabledState, useDeskState } from "../api/deskState";
import { Card, Empty, ErrorNote } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";
import { SeriesChart, type ChartLine } from "../components/SeriesChart";

const RANGES = [
  { label: "6h", hours: 6 },
  { label: "24h", hours: 24 },
  { label: "7d", hours: 24 * 7 },
  { label: "30d", hours: 24 * 30 },
];

export function History() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  const [hours, setHours] = useState(24);

  const snapshots = useDeskHistory<SnapshotPoint>("snapshots", hours, {
    enabled: Boolean(state),
  });
  const symbols = useDeskHistory<SymbolPoint>("symbols", hours, { enabled: Boolean(state) });
  const venues = useDeskHistory<VenuePoint>("venues", hours, { enabled: Boolean(state) });
  const pnl = useDeskHistory<PnlPoint>("pnl", hours, { enabled: Boolean(state) });

  const pnlLines = useMemo(() => cumulativePnlLines(pnl.data?.points ?? []), [pnl.data]);

  const rangePicker = (
    <span style={{ display: "inline-flex", gap: 4 }}>
      {RANGES.map((r) => (
        <button
          key={r.hours}
          className="dash-btn"
          style={hours === r.hours ? { borderColor: "var(--aqua-sui)" } : undefined}
          onClick={() => setHours(r.hours)}
        >
          {r.label}
        </button>
      ))}
    </span>
  );

  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;
  const dec = state.vault.settlementDecimals;
  const scale = 10 ** dec;
  const snapPoints = snapshots.data?.points ?? [];

  return (
    <div className="dash-grid">
      <Card title="Capital" sub="NAV / deployed / reserved (settlement units)" actions={rangePicker}>
        {snapshots.isError ? (
          <ErrorNote error={snapshots.error} what="history" />
        ) : snapPoints.length === 0 ? (
          <Empty>No samples in range.</Empty>
        ) : (
          <SeriesChart
            lines={[
              { name: "NAV", points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.nav / scale })) },
              {
                name: "deployed",
                points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.deployed / scale })),
              },
              {
                name: "reserved",
                points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.reserved / scale })),
              },
            ]}
          />
        )}
      </Card>

      <Card title="Limit utilization" sub="1.0 = soft limit" span="half">
        <SeriesChart
          lines={[
            {
              name: "premium",
              points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.premiumUtil })),
            },
            { name: "vega", points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.vegaUtil })) },
            {
              name: "theta",
              points: snapPoints.map((p) => ({ timeMs: p.timeMs, value: p.thetaUtil })),
            },
          ]}
        />
      </Card>

      <Card title="P&L attribution" sub="cumulative by line, from the durable ledger" span="half">
        {pnl.isError ? (
          <ErrorNote error={pnl.error} what="pnl history" />
        ) : pnlLines.every((l) => l.points.length === 0) ? (
          <Empty>No P&L records in range.</Empty>
        ) : (
          <SeriesChart
            lines={pnlLines.map((l) => ({
              ...l,
              points: l.points.map((p) => ({ ...p, value: p.value / scale })),
            }))}
          />
        )}
      </Card>

      <Card title="Delta vs hedge" sub="per underlying, raw units" span="half">
        {symbols.isError ? (
          <ErrorNote error={symbols.error} what="symbol history" />
        ) : (
          <SeriesChart lines={symbolLines(symbols.data?.points ?? [])} />
        )}
      </Card>

      <Card title="Margin headroom & funding" sub="per venue (min headroom aggregated)" span="half">
        {venues.isError ? (
          <ErrorNote error={venues.error} what="venue history" />
        ) : (
          <SeriesChart digits={3} lines={venueLines(venues.data?.points ?? [])} />
        )}
      </Card>
    </div>
  );
}

function cumulativePnlLines(points: PnlPoint[]): ChartLine[] {
  const byLine = new Map<string, Array<{ timeMs: number; value: number }>>();
  const running = new Map<string, number>();
  for (const p of [...points].sort((a, b) => a.timeMs - b.timeMs)) {
    const total = (running.get(p.line) ?? 0) + p.amount;
    running.set(p.line, total);
    const arr = byLine.get(p.line) ?? [];
    arr.push({ timeMs: p.timeMs, value: total });
    byLine.set(p.line, arr);
  }
  return ["spread", "scalp", "theta", "funding"].map((name) => ({
    name,
    points: byLine.get(name) ?? [],
  }));
}

function symbolLines(points: SymbolPoint[]): ChartLine[] {
  const symbols = [...new Set(points.map((p) => p.symbol))];
  return symbols.flatMap((sym) => {
    const mine = points.filter((p) => p.symbol === sym);
    return [
      {
        name: `${sym} book Δ`,
        points: mine.map((p) => ({ timeMs: p.timeMs, value: p.bookDeltaUnits })),
      },
      {
        name: `${sym} hedge position`,
        points: mine.map((p) => ({ timeMs: p.timeMs, value: p.hedgeUnits })),
      },
      {
        name: `${sym} net`,
        points: mine.map((p) => ({ timeMs: p.timeMs, value: p.netDeltaUnits })),
      },
    ];
  });
}

function venueLines(points: VenuePoint[]): ChartLine[] {
  const keys = [...new Set(points.map((p) => `${p.venue}/${p.symbol}`))];
  return keys.flatMap((key) => {
    const mine = points.filter((p) => `${p.venue}/${p.symbol}` === key);
    return [
      {
        name: `${key} headroom`,
        points: mine.map((p) => ({ timeMs: p.timeMs, value: p.marginHeadroom })),
      },
      {
        name: `${key} funding`,
        points: mine.map((p) => ({ timeMs: p.timeMs, value: p.fundingRateAnnual })),
      },
    ];
  });
}
