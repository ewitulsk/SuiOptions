// Detail panel for the Buy screen (SO-225): Greeks · Details · Book share one
// card in the left column under the strike chain. Absorbs the old
// OptionMetricsPanel's greeks grid and folds in the order book.
//
// The greeks tab shows only the textbook option greeks (delta, gamma, theta,
// vega). The details tab carries the rest of the marks — mid, spot, break even,
// implied vol — plus the position-economics rows once the wallet holds a lot.
// Both come from `/options/metrics`.

import { type ReactNode } from "react";

import type { Bucket, Series } from "../api/client";
import { useCallTokenLots } from "../api/useCallTokenLots";
import { useOptionMetrics } from "../api/optionMetrics";
import { usePositionEconomics, useDayChange } from "../api/positionEconomics";
import { formatPrice } from "../format";
import { useSegmentPill } from "../lib/useSegmentPill";
import { Orderbook } from "./Orderbook";
import { OpenOrdersSection } from "./OpenOrdersSection";
import { WaveLoader } from "./WaveLoader";

export type DetailTab = "greeks" | "details" | "book" | "orders";

type Props = {
  bucket: Bucket;
  series: Series;
  spot: number;
  mid: number | null;
  wallet: string | null;
  // Controlled by the parent so the active tab survives strike-grid clicks
  // (which remount this component via its `key`).
  tab: DetailTab;
  onTabChange: (tab: DetailTab) => void;
};

const usd = (n: number) => `$${formatPrice(n, { grouping: true })}`;
const pct = (n: number) => `${(n * 100).toFixed(2)}%`;
const qtyFmt = (n: number) =>
  n.toLocaleString("en-US", { minimumFractionDigits: 0, maximumFractionDigits: 4 });
const tone = (n: number) => (n >= 0 ? "pos" : "neg");

function Cell({
  label,
  tone: t,
  className,
  children,
}: {
  label: string;
  tone?: "pos" | "neg";
  className?: string;
  children: ReactNode;
}) {
  const cls =
    "exercise__value" +
    (t === "pos" ? " exercise__value--pos" : t === "neg" ? " exercise__value--neg" : "");
  return (
    <div className={"exercise__cell" + (className ? ` ${className}` : "")}>
      <div className="exercise__label">{label}</div>
      <div className={cls}>{children}</div>
    </div>
  );
}

export function BuyDetailTabs({ bucket, series, spot, mid, wallet, tab, onTabChange }: Props) {
  const strike = bucket.strike;

  const metrics = useOptionMetrics({ strike, expiryMs: series.expiry_ms, spot, mid });
  const lotsQuery = useCallTokenLots(wallet);
  const lots = lotsQuery.data ?? [];
  const econ = usePositionEconomics(lots, bucket.bucket_id, mid);
  const day = useDayChange(bucket.deepbook_pool_id, econ.qty, mid);

  const m = metrics.data;
  const greekLoading = metrics.isFetching && !m;
  const greek = (v: ReactNode) => (greekLoading ? <WaveLoader /> : m ? v : "—");
  const hasPosition = econ.qty > 0;

  const { ref: barRef, geom: pill, animated } = useSegmentPill(tab);

  return (
    <div className="panel dtabs">
      <div className="dtabs__bar" role="tablist" ref={barRef}>
        <span
          className="dtabs__pill"
          aria-hidden
          style={{
            transform: `translateX(${pill.left}px)`,
            width: pill.width,
            opacity: pill.ready ? 1 : 0,
            transition: animated ? undefined : "none",
          }}
        />
        {(["greeks", "details", "book", "orders"] as DetailTab[]).map((t) => (
          <button
            key={t}
            role="tab"
            aria-selected={tab === t}
            className={"dtabs__tab" + (tab === t ? " is-active" : "")}
            onClick={() => onTabChange(t)}
          >
            {t}
          </button>
        ))}
      </div>

      {tab === "greeks" && (
        <div className="exercise dtabs__grid">
          <Cell label="delta">{greek(m && m.delta.toFixed(3))}</Cell>
          <Cell label="gamma">{greek(m && m.gamma.toFixed(4))}</Cell>
          <Cell label="theta / day">{greek(m && usd(m.theta))}</Cell>
          <Cell label="vega / 1%">{greek(m && usd(m.vega / 100))}</Cell>
        </div>
      )}

      {tab === "details" && (
        <div className="exercise dtabs__grid">
          {hasPosition && (
            <>
              <Cell label="open p&l" tone={tone(econ.openPnl)} className="pnl-swap" >
                <span className="pnl-swap__val">{usd(econ.openPnl)}</span>
                <span className="pnl-swap__pct">{pct(econ.openPnlPct)}</span>
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
            </>
          )}

          <Cell label="mid price">{mid != null ? usd(mid) : "—"}</Cell>
          <Cell label={`spot · ${series.asset_symbol}`}>{spot > 0 ? usd(spot) : "—"}</Cell>
          <Cell label="break even">{greek(m && usd(m.break_even))}</Cell>
          <Cell label="impl. vol.">
            {greek(m && (m.implied_vol != null ? pct(m.implied_vol) : "—"))}
          </Cell>
        </div>
      )}

      {tab === "book" && (
        <div className="dtabs__book">
          <Orderbook bucket={bucket} series={series} />
        </div>
      )}

      {tab === "orders" && (
        <div className="dtabs__orders">
          <OpenOrdersSection bucket={bucket} series={series} />
        </div>
      )}
    </div>
  );
}
