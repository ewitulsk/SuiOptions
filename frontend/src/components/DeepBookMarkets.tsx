// Debug view of every DeepBook pool on our deployment.
//
// The deployment is shared by staging and prod and outlives contract
// redeploys, so `PoolCreated` enumerates hundreds of pools — most of them
// option venues whose bucket died with an old deployment. Rows are classified:
//
//   option — base coin is an option coin of a currently-indexed bucket
//   spot   — both sides are token-info catalog / test tokens
//   stale  — anything else, i.e. an option pool from a past deployment
//
// Only live rows (option + spot) render by default; "show historical" widens
// the set and slows the poll, since every extra 20 pools costs one simulate.
// Classification deliberately needs no coin metadata, so the visible set is
// known before the (rate-limited) `coinMetadata` reads that supply symbols and
// the decimals every price is scaled by.

import { useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { normalizeStructTag } from "@mysten/sui/utils";

import { optionCoinType, seriesOptionType, type Bucket, type Series } from "../api/client";
import { useBuckets } from "../api/useBuckets";
import {
  useCoinMeta,
  useDeepBookMarkets,
  useOrderBooks,
  type CoinMeta,
  type DeepBookMarket,
  type RawBook,
  type RawLevel,
} from "../api/deepbookMarkets";
import { SUPPORTED_TOKENS, TEST_TOKENS } from "../config";
import { formatPrice } from "../format";
import { fromRawPrice } from "../tx/deepbook";

type Kind = "option" | "spot" | "stale";

type ClassifiedMarket = DeepBookMarket & {
  kind: Kind;
  /** Bucket-derived name for option pools, e.g. `TBTC 120,000 C · Aug 1`. */
  optionLabel: string | null;
  /** Debug annotation, e.g. a live bucket whose pool the indexer never tied. */
  note: string | null;
};

const KIND_ORDER: Record<Kind, number> = { option: 0, spot: 1, stale: 2 };

export function DeepBookMarkets() {
  const account = useCurrentAccount();
  const viewer = account?.address ?? null;
  const markets = useDeepBookMarkets();
  const buckets = useBuckets();
  const [showAll, setShowAll] = useState(false);
  const [filter, setFilter] = useState("");

  const all = useMemo(() => markets.data ?? [], [markets.data]);
  const classified = useMemo(() => classify(all, buckets.data ?? []), [all, buckets.data]);

  const visible = useMemo(() => {
    const base = showAll ? classified : classified.filter((m) => m.kind !== "stale");
    const q = filter.trim().toLowerCase();
    if (!q) return base;
    return base.filter(
      (m) =>
        m.optionLabel?.toLowerCase().includes(q) ||
        m.poolId.toLowerCase().includes(q) ||
        m.baseType.toLowerCase().includes(q) ||
        m.quoteType.toLowerCase().includes(q),
    );
  }, [classified, showAll, filter]);

  const meta = useCoinMeta(useMemo(() => visible.flatMap((m) => [m.baseType, m.quoteType]), [visible]));
  // "Show historical" adds ~16 more simulate batches per pass — poll it slower.
  const books = useOrderBooks(visible, viewer, showAll ? 20_000 : 5_000);

  const liveCount = classified.filter((m) => m.kind !== "stale").length;
  const staleCount = classified.length - liveCount;

  return (
    <section className="live-buckets">
      <h3 className="live-buckets__title">DeepBook order books</h3>

      {markets.isLoading && <div className="live-buckets__status">discovering pools…</div>}
      {markets.error && (
        <div className="live-buckets__status live-buckets__status--error">
          pool discovery failed: {markets.error.message}
        </div>
      )}
      {meta.error && (
        <div className="live-buckets__status live-buckets__status--error">
          coin metadata failed: {meta.error.message} — prices shown raw
        </div>
      )}

      {classified.length > 0 && (
        <>
          <div className="db-markets__bar">
            <span className="live-buckets__status">
              {classified.length} pools · {liveCount} live · {staleCount} historical · books{" "}
              {books.loaded}/{books.batches}
              {books.isFetching ? " · refreshing" : ""}
            </span>
            <input
              className="db-markets__filter"
              placeholder="filter pool id / coin type / strike"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            <label className="db-markets__toggle">
              <input
                type="checkbox"
                checked={showAll}
                onChange={(e) => setShowAll(e.target.checked)}
              />
              show historical ({staleCount})
            </label>
          </div>

          <div className="db-markets__head">
            <span>market</span>
            <span>kind</span>
            <span className="db-markets__num">bid</span>
            <span className="db-markets__num">mid</span>
            <span className="db-markets__num">ask</span>
            <span className="db-markets__num">spread</span>
            <span className="db-markets__num">bid qty</span>
            <span className="db-markets__num">ask qty</span>
          </div>

          {visible.map((m) => (
            <MarketRow
              key={m.poolId}
              market={m}
              base={meta.data?.[m.baseType]}
              quote={meta.data?.[m.quoteType]}
              book={books.books[m.poolId]}
              error={books.errors[m.poolId]}
            />
          ))}
          {visible.length === 0 && <div className="live-buckets__status">no pools match</div>}
        </>
      )}
    </section>
  );
}

function MarketRow({
  market,
  base,
  quote,
  book,
  error,
}: {
  market: ClassifiedMarket;
  base: CoinMeta | undefined;
  quote: CoinMeta | undefined;
  book: RawBook | undefined;
  error: string | undefined;
}) {
  const baseSymbol = base?.symbol ?? shortType(market.baseType);
  const quoteSymbol = quote?.symbol ?? shortType(market.quoteType);
  const label = `${market.optionLabel ?? baseSymbol} / ${quoteSymbol}`;
  const scale = (l: RawLevel) => scaleLevel(l, base?.decimals ?? null, quote?.decimals ?? null);

  const bids = (book?.bids ?? []).map(scale);
  const asks = (book?.asks ?? []).map(scale);
  const bestBid = bids[0]?.price ?? null;
  const bestAsk = asks[0]?.price ?? null;
  const mid = bestBid != null && bestAsk != null ? (bestBid + bestAsk) / 2 : null;
  const spread = bestBid != null && bestAsk != null ? bestAsk - bestBid : null;
  const crossed = bestBid != null && bestAsk != null && bestBid >= bestAsk;
  const bidQty = bids.reduce((a, l) => a + l.qty, 0);
  const askQty = asks.reduce((a, l) => a + l.qty, 0);
  const empty = book != null && bids.length === 0 && asks.length === 0;
  const rawUnits = base == null || quote == null;

  return (
    <details className="db-markets__market">
      <summary className="db-markets__row">
        <span className="db-markets__name" title={`${market.baseType}\n${market.quoteType}`}>
          {label}
          {market.note && <em className="db-markets__note"> · {market.note}</em>}
          {rawUnits && <em className="db-markets__note"> · raw units</em>}
          {crossed && <em className="db-markets__flag"> · crossed</em>}
        </span>
        <span className={`db-markets__kind db-markets__kind--${market.kind}`}>{market.kind}</span>
        <span className="db-markets__num db-markets__bid">{num(bestBid)}</span>
        <span className="db-markets__num">{num(mid)}</span>
        <span className="db-markets__num db-markets__ask">{num(bestAsk)}</span>
        <span className="db-markets__num">
          {spread != null && mid ? `${((spread / mid) * 100).toFixed(2)}%` : "—"}
        </span>
        <span className="db-markets__num">{qty(bidQty, bids.length)}</span>
        <span className="db-markets__num">{qty(askQty, asks.length)}</span>
      </summary>

      <div className="db-markets__detail">
        {error && <div className="live-buckets__status live-buckets__status--error">{error}</div>}
        {!book && !error && <div className="live-buckets__status">loading book…</div>}
        {empty && <div className="live-buckets__status">book is empty</div>}
        {book && !empty && (
          <div className="db-markets__ladder">
            <div className="db-markets__side">
              <div className="db-markets__side-head">bids</div>
              {bids.map((l, i) => (
                <div key={`b${i}`} className="db-markets__level db-markets__bid">
                  <span>{num(l.price)}</span>
                  <span className="db-markets__level-qty">{l.qty.toLocaleString()}</span>
                </div>
              ))}
              {bids.length === 0 && <div className="db-markets__level">—</div>}
            </div>
            <div className="db-markets__side">
              <div className="db-markets__side-head">asks</div>
              {asks.map((l, i) => (
                <div key={`a${i}`} className="db-markets__level db-markets__ask">
                  <span>{num(l.price)}</span>
                  <span className="db-markets__level-qty">{l.qty.toLocaleString()}</span>
                </div>
              ))}
              {asks.length === 0 && <div className="db-markets__level">—</div>}
            </div>
          </div>
        )}
        <dl className="db-markets__params">
          <div>
            <dt>pool</dt>
            <dd title={market.poolId}>{shortHex(market.poolId)}</dd>
          </div>
          <div>
            <dt>base</dt>
            <dd title={market.baseType}>
              {baseSymbol} ({base?.decimals ?? "?"} dp)
            </dd>
          </div>
          <div>
            <dt>quote</dt>
            <dd title={market.quoteType}>
              {quoteSymbol} ({quote?.decimals ?? "?"} dp)
            </dd>
          </div>
          <div>
            <dt>tick / lot / min</dt>
            <dd>
              {market.tickSize} / {market.lotSize} / {market.minSize}
            </dd>
          </div>
          <div>
            <dt>taker / maker fee</dt>
            <dd>
              {feePct(market.takerFee)} / {feePct(market.makerFee)}
            </dd>
          </div>
          <div>
            <dt>whitelisted</dt>
            <dd>{market.whitelisted ? "yes" : "no"}</dd>
          </div>
        </dl>
      </div>
    </details>
  );
}

// ---- classification ----------------------------------------------------------

/**
 * Tag each pool against the live bucket catalog and the token-info catalog.
 * Matching options by **coin type** (not by `deepbook_pool_id`) keeps a live
 * bucket classified as live even when the indexer hasn't tied its pool yet —
 * that gap is exactly what the `note` reports.
 */
function classify(markets: DeepBookMarket[], series: Series[]): ClassifiedMarket[] {
  const options = new Map<string, { bucket: Bucket; series: Series }>();
  for (const s of series) {
    for (const b of s.buckets) {
      options.set(normalizeStructTag(optionCoinType(b)), { bucket: b, series: s });
    }
  }
  const catalog = new Set<string>([
    ...SUPPORTED_TOKENS.map((t) => normalizeStructTag(t.coinType)),
    ...TEST_TOKENS.map((t) => normalizeStructTag(t.coinType)),
  ]);

  const out = markets.map((m): ClassifiedMarket => {
    const base = normalizeStructTag(m.baseType);
    const quote = normalizeStructTag(m.quoteType);
    const hit = options.get(base);
    if (hit) {
      let note: string | null = null;
      if (!hit.bucket.deepbook_pool_id) note = "bucket has no indexed pool";
      else if (normalizeSuiId(hit.bucket.deepbook_pool_id) !== normalizeSuiId(m.poolId))
        note = "second pool for this bucket";
      return { ...m, kind: "option", optionLabel: bucketLabel(hit.bucket, hit.series), note };
    }
    const kind: Kind = catalog.has(base) && catalog.has(quote) ? "spot" : "stale";
    return { ...m, kind, optionLabel: null, note: null };
  });

  return out.sort(
    (a, b) =>
      KIND_ORDER[a.kind] - KIND_ORDER[b.kind] ||
      (a.optionLabel ?? a.baseType).localeCompare(b.optionLabel ?? b.baseType),
  );
}

function bucketLabel(b: Bucket, s: Series): string {
  const side = seriesOptionType(s) === "put" ? "P" : "C";
  // Enough precision that neighbouring strikes on a cheap underlying (TWAL at
  // ~$0.025) stay distinguishable in the row label.
  const strike =
    b.strike === null
      ? b.strike_raw
      : b.strike.toLocaleString(undefined, { maximumFractionDigits: 6 });
  const expiry = new Date(s.expiry_ms);
  const when = Number.isNaN(expiry.getTime())
    ? s.expiry_iso
    : expiry.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  return `${s.asset_symbol} ${strike} ${side} · ${when}`;
}

// ---- formatting ---------------------------------------------------------------

/**
 * Raw level → display units. Without coin metadata the decimals are unknown, so
 * the raw integers pass through rather than being silently mis-scaled (the row
 * says "raw units" when that happens).
 */
function scaleLevel(
  l: RawLevel,
  baseDecimals: number | null,
  quoteDecimals: number | null,
): { price: number; qty: number } {
  if (baseDecimals == null || quoteDecimals == null) {
    return { price: Number(l.priceRaw), qty: Number(l.qtyRaw) };
  }
  return {
    price: fromRawPrice(BigInt(l.priceRaw), baseDecimals, quoteDecimals),
    qty: Number(l.qtyRaw) / 10 ** baseDecimals,
  };
}

function num(v: number | null): string {
  return v == null ? "—" : formatPrice(v);
}

function qty(total: number, levels: number): string {
  if (levels === 0) return "—";
  return `${total.toLocaleString(undefined, { maximumFractionDigits: 4 })} (${levels})`;
}

/** DeepBook fees are billionths of the traded amount. */
function feePct(raw: string): string {
  return `${((Number(raw) / 1e9) * 100).toFixed(3)}%`;
}

function shortType(t: string): string {
  const parts = t.split("::");
  return parts[parts.length - 1] ?? t;
}

function shortHex(s: string): string {
  return s.length <= 12 ? s : `${s.slice(0, 6)}…${s.slice(-4)}`;
}

/** Object ids arrive both zero-padded and not; compare on the trimmed form. */
function normalizeSuiId(id: string): string {
  return id.replace(/^0x0*/, "").toLowerCase();
}
