// Standalone exchange order book (SO-170/SO-416): lives in the Buy screen's
// detail tabs. Shares the `["exchange-book", marketId]` query with
// TradePanel's market estimate, so React Query serves both from one fetch.

import type { Bucket, Series } from "../api/client";
import {
  midFromBook,
  toDisplayBook,
  useExchangeBook,
  useExchangeMarketFor,
} from "../api/orderbook";
import { formatPrice } from "../format";

type Props = {
  bucket: Bucket;
  series: Series;
};

export function Orderbook({ bucket, series }: Props) {
  const { market } = useExchangeMarketFor(bucket);
  const bookQ = useExchangeBook(market?.registryId ?? null);
  const book = toDisplayBook(
    bookQ.data,
    market,
    series.asset_decimals ?? 8,
    series.settlement_decimals ?? 6,
  );

  if (!market) return null;

  const empty = (book?.asks?.length ?? 0) + (book?.bids?.length ?? 0) === 0;
  const mid = midFromBook(book);

  return (
    <div className="orderbook">
      <div className="panel__head">
        order book
        <span className="orderbook__mid-label">
          mid · {mid != null ? formatPrice(mid) : "—"}
        </span>
      </div>
      <div className="orderbook__rows">
        {(book?.asks ?? []).slice(0, 8).reverse().map((l, i) => (
          <div key={`a${i}`} className="orderbook__row orderbook__row--ask">
            <span>{formatPrice(l.price)}</span>
            <span className="orderbook__qty">{l.qty}</span>
          </div>
        ))}
        <div className="orderbook__mid" />
        {(book?.bids ?? []).slice(0, 8).map((l, i) => (
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
