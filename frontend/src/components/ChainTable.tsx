// Dense options-chain table for the Buy screen (SO-225): replaces the sparse
// BucketList strike rail with a Deribit/Aevo-style chain — one row per strike
// showing live Bid · Mark · Ask plus model IV · Δ, with a spot-price divider
// threaded through the strikes. Calls only (this protocol has no puts), so it's
// a one-sided chain.
//
// Each row owns its own book + greeks hooks so the whole chain stays live; both
// poll gently (the selected strike keeps the fast cadence via the ticket/center
// panels) to avoid a devInspect-per-strike storm.

import type { Bucket, Series } from "../api/client";
import {
  midFromBook,
  toDisplayBook,
  useExchangeBook,
  useExchangeMarketFor,
} from "../api/orderbook";
import { useOptionMetrics } from "../api/optionMetrics";
import { formatPrice } from "../format";
import type { Strike } from "../types";

// Slow poll for the off-screen breadth of the chain. The selected market is
// also observed by the ticket/center at the default 3s, so it stays fresh
// regardless.
const ROW_BOOK_MS = 15_000;
const ROW_METRICS_MS = 15_000;

function fmtStrike(s: number): string {
  return `$${s.toLocaleString("en-US", { maximumFractionDigits: s % 1 === 0 ? 0 : 2 })}`;
}

type RowProps = {
  bucket: Bucket;
  series: Series;
  spot: number;
  /** Indicative premium (bulk-view) — the Mark fallback when there's no book. */
  mark: number;
  selected: boolean;
  onSelect: () => void;
};

function ChainRow({ bucket, series, spot, mark, selected, onSelect }: RowProps) {
  const { market } = useExchangeMarketFor(bucket);
  const bookQ = useExchangeBook(market?.registryId ?? null, 8, ROW_BOOK_MS);
  const book = toDisplayBook(
    bookQ.data,
    market,
    series.asset_decimals ?? 8,
    series.settlement_decimals ?? 6,
  );
  const bid = book?.bids[0]?.price ?? null;
  const ask = book?.asks[0]?.price ?? null;
  const mid = midFromBook(book);
  const markPrice = mid ?? (mark > 0 ? mark : null);

  const metrics = useOptionMetrics({
    strike: bucket.strike,
    expiryMs: series.expiry_ms,
    spot,
    mid: markPrice,
    refetchInterval: ROW_METRICS_MS,
  });
  const m = metrics.data;

  const num = (v: number | null, fn: (n: number) => string) => (v != null ? fn(v) : "—");

  return (
    <button
      className={"chain__row" + (selected ? " is-selected" : "")}
      onClick={onSelect}
    >
      <span className="chain__strike">{fmtStrike(bucket.strike ?? 0)}</span>
      <span className="chain__bid">{num(bid, formatPrice)}</span>
      <span className="chain__mark">{num(markPrice, formatPrice)}</span>
      <span className="chain__ask">{num(ask, formatPrice)}</span>
      <span className="chain__iv">{m?.implied_vol != null ? `${(m.implied_vol * 100).toFixed(0)}%` : "—"}</span>
      <span className="chain__delta">{m ? m.delta.toFixed(2) : "—"}</span>
    </button>
  );
}

type Props = {
  buckets: Bucket[];
  strikes: Strike[];
  series: Series;
  spot: number;
  selectedIdx: number;
  onSelect: (i: number) => void;
};

export function ChainTable({ buckets, strikes, series, spot, selectedIdx, onSelect }: Props) {
  // Strikes are ascending; the divider sits before the first strike above spot.
  const dividerIdx =
    spot > 0 ? buckets.findIndex((b) => (b.strike ?? 0) > spot) : -1;

  return (
    <div className="chain">
      <div className="chain__head">
        <span>strike</span>
        <span>bid</span>
        <span>mark</span>
        <span>ask</span>
        <span>iv</span>
        <span>Δ</span>
      </div>
      <div className="chain__rows">
        {buckets.map((b, i) => (
          <div key={`${i}-${b.bucket_id}`} style={{ display: "contents" }}>
            {i === dividerIdx && (
              <div className="chain__divider">
                <span>{series.asset_symbol} ${formatPrice(spot, { grouping: true })}</span>
              </div>
            )}
            <ChainRow
              bucket={b}
              series={series}
              spot={spot}
              mark={strikes[i]?.premium ?? 0}
              selected={selectedIdx === i}
              onSelect={() => onSelect(i)}
            />
          </div>
        ))}
        {dividerIdx === -1 && spot > 0 && buckets.length > 0 && (
          <div className="chain__divider">
            <span>{series.asset_symbol} ${formatPrice(spot, { grouping: true })}</span>
          </div>
        )}
      </div>
    </div>
  );
}
