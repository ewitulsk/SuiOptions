// Standalone DeepBook order book (SO-170): extracted from TradePanel so it can
// live in the Buy screen's right rail. Shares the `["deepbook-book", poolId]`
// query with TradePanel's market estimate, so React Query serves both from one
// fetch.

import { useCurrentAccount } from "@mysten/dapp-kit";

import type { Bucket, Series } from "../api/client";
import { midFromBook, poolRefFor, useOrderBook } from "../api/deepbook";
import { formatPrice } from "../format";

type Props = {
  bucket: Bucket;
  series: Series;
};

export function Orderbook({ bucket, series }: Props) {
  const account = useCurrentAccount();
  const pool = poolRefFor(bucket, series);
  const book = useOrderBook(pool, account?.address ?? null);

  if (!pool) return null;

  const empty = (book.data?.asks?.length ?? 0) + (book.data?.bids?.length ?? 0) === 0;
  const mid = midFromBook(book.data);

  return (
    <div className="panel orderbook">
      <div className="panel__head">
        <span className="panel__head-dot"></span>order book
        <span className="orderbook__mid-label">
          mid · {mid != null ? formatPrice(mid) : "—"}
        </span>
      </div>
      <div className="orderbook__rows">
        {(book.data?.asks ?? []).slice(0, 8).reverse().map((l, i) => (
          <div key={`a${i}`} className="orderbook__row orderbook__row--ask">
            <span>{formatPrice(l.price)}</span>
            <span className="orderbook__qty">{l.qty}</span>
          </div>
        ))}
        <div className="orderbook__mid" />
        {(book.data?.bids ?? []).slice(0, 8).map((l, i) => (
          <div key={`b${i}`} className="orderbook__row orderbook__row--bid">
            <span>{formatPrice(l.price)}</span>
            <span className="orderbook__qty">{l.qty}</span>
          </div>
        ))}
        {empty && <div className="panel__sub">book is empty</div>}
      </div>
    </div>
  );
}
