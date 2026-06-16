// Option-detail metrics panel for the Buy page (SO-199): a Robinhood/Webull-
// style stat grid plus the long-call payoff diagram. Greeks/IV come from the
// `/options/metrics` endpoint (SO-198); everything else is a pure client
// derivation of data the browser already holds (lots, order-book mid, spot).
//
// Reuses the card + stat-cell styling from `Panels.tsx` (`panel`,
// `exercise`/`exercise__cell`). Position-economics rows are hidden until the
// wallet holds a lot for this bucket — pre-purchase this is a quote view.

import type { ReactNode } from "react";

import type { Bucket, Series } from "../api/client";
import { useCallTokenLots } from "../api/useCallTokenLots";
import { useOptionMetrics } from "../api/optionMetrics";
import { usePositionEconomics, useDayChange } from "../api/positionEconomics";
import { formatPrice } from "../format";
import { PayoffChart } from "./PayoffChart";
import { WaveLoader } from "./WaveLoader";

type Props = {
  bucket: Bucket;
  series: Series;
  spot: number;
  mid: number | null;
  /** Connected wallet / session address, for lot lookup. `null` if signed out. */
  wallet: string | null;
};

const usd = (n: number) => `$${formatPrice(n, { grouping: true })}`;
const pct = (n: number) => `${(n * 100).toFixed(2)}%`;
const qtyFmt = (n: number) =>
  n.toLocaleString("en-US", { minimumFractionDigits: 0, maximumFractionDigits: 4 });
const tone = (n: number) => (n >= 0 ? "pos" : "neg");

function Cell({
  label,
  tone: t,
  children,
}: {
  label: string;
  tone?: "pos" | "neg";
  children: ReactNode;
}) {
  const cls =
    "exercise__value" +
    (t === "pos" ? " exercise__value--pos" : t === "neg" ? " exercise__value--neg" : "");
  return (
    <div className="exercise__cell">
      <div className="exercise__label">{label}</div>
      <div className={cls}>{children}</div>
    </div>
  );
}

export function OptionMetricsPanel({ bucket, series, spot, mid, wallet }: Props) {
  const strike = bucket.strike;
  const metrics = useOptionMetrics({
    strike,
    expiryMs: series.expiry_ms,
    spot,
    mid,
  });
  const lotsQuery = useCallTokenLots(wallet);
  const lots = lotsQuery.data ?? [];
  const econ = usePositionEconomics(lots, bucket.bucket_id, mid);
  const day = useDayChange(bucket.deepbook_pool_id, econ.qty, mid);

  const m = metrics.data;
  // Distinguish "fetching, no data yet" (show motion) from "disabled / no mid"
  // (show an em-dash) — a disabled query stays pending but never fetches.
  const greekLoading = metrics.isFetching && !m;
  const greek = (v: ReactNode) => (greekLoading ? <WaveLoader /> : m ? v : "—");

  const hasPosition = econ.qty > 0;

  return (
    <>
      <div className="panel" style={{ marginTop: 12 }}>
        <div className="panel__head">
          <span className="panel__head-dot"></span>option metrics
        </div>
        <div className="exercise" style={{ marginBottom: 0 }}>
          {hasPosition && (
            <>
              <Cell label="open p&l" tone={tone(econ.openPnl)}>
                {usd(econ.openPnl)}
                <span style={{ fontSize: 12, marginLeft: 6 }}>{pct(econ.openPnlPct)}</span>
              </Cell>
              <Cell
                label="day's p&l"
                tone={day.optionPnl != null ? tone(day.optionPnl) : undefined}
              >
                {day.optionPnl != null ? usd(day.optionPnl) : "—"}
              </Cell>
              <Cell label="market value">{usd(econ.marketValue)}</Cell>
              <Cell label="total cost">{usd(econ.totalCost)}</Cell>
              <Cell label="avg price">{usd(econ.avgPrice)}</Cell>
              <Cell label="qty in position">{qtyFmt(econ.qty)}</Cell>
              <Cell label="position ratio">{pct(econ.positionRatio)}</Cell>
            </>
          )}

          <Cell label="mid price">{mid != null ? usd(mid) : "—"}</Cell>
          <Cell label={`spot · ${series.asset_symbol}`}>{spot > 0 ? usd(spot) : "—"}</Cell>
          <Cell label="delta">{greek(m && m.delta.toFixed(3))}</Cell>
          <Cell label="theta / day">{greek(m && usd(m.theta))}</Cell>
          <Cell label="break even">{greek(m && usd(m.break_even))}</Cell>
          <Cell label="impl. vol.">
            {greek(m && (m.implied_vol != null ? pct(m.implied_vol) : "—"))}
          </Cell>
        </div>
      </div>

      {hasPosition && strike != null && (
        <PayoffChart
          strike={strike}
          qty={econ.qty}
          totalCost={econ.totalCost}
          breakEven={strike + econ.avgPrice}
          spot={spot}
        />
      )}
    </>
  );
}
