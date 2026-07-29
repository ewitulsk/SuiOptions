// Debug-page reads over EVERY pool on our DeepBook deployment, not just the
// ones the api-service ties to a live bucket.
//
// Pool discovery goes through `pool::PoolCreated` events on the DeepBook
// original package (the registry's pool Bag isn't readable without knowing the
// pair up front). The deployment is shared between staging and prod and
// survives contract redeploys, so this list also contains every option pool
// from past deployments — `Debug`'s market table classifies them.
//
// Books come back RAW (u64 price/quantity straight off the chain) so the
// coin-decimals lookup can arrive independently without invalidating them; the
// component scales at render time.

import { useMemo } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { bcs } from "@mysten/sui/bcs";
import type { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { DEEPBOOK_ORIGINAL_PACKAGE_ID, DEEPBOOK_PACKAGE_ID } from "../config";
import { suiGraphqlQuery, useSuiGrpcClient, useSuiNetwork, type SuiNetwork } from "../lib/suiGrpc";
import { devInspect } from "./deepbook";

/** Ticks per side pulled from mid — matches the trading order book widget. */
const BOOK_TICKS = 8n;
/** Pools per `SimulateTransaction`. One command each; the whole batch shares a round trip. */
const BOOK_BATCH = 20;
// `coinMetadata` aliases per GraphQL request. The RPC rejects a request whose
// store-backed sub-queries exceed 21, and each `coinMetadata` costs three, so
// seven is the ceiling — verified against graphql.testnet.sui.io. Batches run a
// few at a time so a wide market list doesn't serialize 50 round trips.
const META_BATCH = 7;
const META_CONCURRENCY = 4;

const VEC_U64 = bcs.vector(bcs.u64());

/** One DeepBook pool as reported by its creation event. */
export type DeepBookMarket = {
  poolId: string;
  baseType: string;
  quoteType: string;
  tickSize: string;
  lotSize: string;
  minSize: string;
  takerFee: string;
  makerFee: string;
  whitelisted: boolean;
};

/** A book level before decimals are known. */
export type RawLevel = { priceRaw: string; qtyRaw: string };
export type RawBook = { bids: RawLevel[]; asks: RawLevel[] };

// ---- pool discovery -----------------------------------------------------------

type PoolEventsPage = {
  events: {
    pageInfo: { hasPreviousPage: boolean; startCursor: string | null };
    nodes: Array<{
      contents: {
        type: { repr: string } | null;
        json: {
          pool_id?: string;
          tick_size?: string;
          lot_size?: string;
          min_size?: string;
          taker_fee?: string;
          maker_fee?: string;
          whitelisted_pool?: boolean;
        } | null;
      } | null;
    }>;
  };
};

// A `type:` filter without generics prefix-matches every `PoolCreated<B, Q>`.
// `last:` + `before:` walks newest-first.
const POOL_EVENTS_QUERY = `
  query($type: String!, $before: String) {
    events(last: 50, before: $before, filter: { type: $type }) {
      pageInfo { hasPreviousPage startCursor }
      nodes { contents { type { repr } json } }
    }
  }`;

/** Split `A, B` at the top level — coin types can themselves be generic. */
function splitGenerics(inner: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < inner.length; i++) {
    const c = inner[i];
    if (c === "<") depth++;
    else if (c === ">") depth--;
    else if (c === "," && depth === 0) {
      out.push(inner.slice(start, i).trim());
      start = i + 1;
    }
  }
  out.push(inner.slice(start).trim());
  return out;
}

async function fetchDeepBookMarkets(network: SuiNetwork): Promise<DeepBookMarket[]> {
  const pkg = DEEPBOOK_ORIGINAL_PACKAGE_ID ?? DEEPBOOK_PACKAGE_ID;
  if (!pkg) return [];

  const seen = new Set<string>();
  const out: DeepBookMarket[] = [];
  let before: string | null = null;
  // 40 pages × 50 = 2 000 pools; far above the few hundred a deployment holds,
  // and a hard stop so a broken cursor can't spin forever.
  for (let page = 0; page < 40; page++) {
    const res: PoolEventsPage = await suiGraphqlQuery<PoolEventsPage>(network, POOL_EVENTS_QUERY, {
      type: `${pkg}::pool::PoolCreated`,
      before,
    });
    for (const node of res.events.nodes) {
      const json = node.contents?.json;
      const repr = node.contents?.type?.repr;
      if (!json?.pool_id || !repr) continue;
      const open = repr.indexOf("<");
      if (open < 0 || !repr.endsWith(">")) continue;
      const [baseType, quoteType] = splitGenerics(repr.slice(open + 1, -1));
      if (!baseType || !quoteType) continue;
      if (seen.has(json.pool_id)) continue;
      seen.add(json.pool_id);
      out.push({
        poolId: json.pool_id,
        baseType,
        quoteType,
        tickSize: json.tick_size ?? "0",
        lotSize: json.lot_size ?? "0",
        minSize: json.min_size ?? "0",
        takerFee: json.taker_fee ?? "0",
        makerFee: json.maker_fee ?? "0",
        whitelisted: json.whitelisted_pool ?? false,
      });
    }
    if (!res.events.pageInfo.hasPreviousPage || !res.events.pageInfo.startCursor) break;
    before = res.events.pageInfo.startCursor;
  }
  return out;
}

/** Every pool ever created on our DeepBook deployment, newest first. */
export function useDeepBookMarkets() {
  const network = useSuiNetwork();
  return useQuery<DeepBookMarket[], Error>({
    queryKey: ["deepbook-markets", network],
    enabled: Boolean(DEEPBOOK_ORIGINAL_PACKAGE_ID ?? DEEPBOOK_PACKAGE_ID),
    // Pools only appear when someone creates one — a slow poll is plenty.
    refetchInterval: 60_000,
    staleTime: 30_000,
    queryFn: () => fetchDeepBookMarkets(network),
  });
}

// ---- coin metadata --------------------------------------------------------------

export type CoinMeta = { symbol: string; decimals: number };

/**
 * `CoinMetadata` for arbitrary coin types, keyed by the type string exactly as
 * the pool event reported it. Option coins from dead deployments still resolve
 * (packages are immutable), which is what makes the historical pools readable.
 */
async function fetchCoinMeta(
  network: SuiNetwork,
  coinTypes: string[],
): Promise<Record<string, CoinMeta>> {
  const batches: string[][] = [];
  for (let i = 0; i < coinTypes.length; i += META_BATCH) {
    batches.push(coinTypes.slice(i, i + META_BATCH));
  }

  const out: Record<string, CoinMeta> = {};
  for (let i = 0; i < batches.length; i += META_CONCURRENCY) {
    const wave = batches.slice(i, i + META_CONCURRENCY).map(async (batch) => {
      const args = batch.map((_, j) => `$c${j}: String!`).join(", ");
      const fields = batch.map(
        (_, j) => `m${j}: coinMetadata(coinType: $c${j}) { symbol decimals }`,
      );
      const query = `query(${args}) { ${fields.join(" ")} }`;
      const vars = Object.fromEntries(batch.map((t, j) => [`c${j}`, t]));
      const data = await suiGraphqlQuery<Record<string, CoinMeta | null>>(network, query, vars);
      batch.forEach((t, j) => {
        const m = data[`m${j}`];
        // A coin with no published metadata just stays unresolved.
        if (m) out[t] = m;
      });
    });
    await Promise.all(wave);
  }
  return out;
}

export function useCoinMeta(coinTypes: string[]) {
  const network = useSuiNetwork();
  // Stable, de-duplicated list so the query key is order-independent.
  const types = Array.from(new Set(coinTypes)).sort();
  return useQuery<Record<string, CoinMeta>, Error>({
    queryKey: ["deepbook-coin-meta", network, types.join(",")],
    enabled: types.length > 0,
    // Coin metadata is immutable in practice; never re-poll.
    staleTime: Infinity,
    queryFn: () => fetchCoinMeta(network, types),
  });
}

// ---- books --------------------------------------------------------------------

/**
 * One `get_level2_ticks_from_mid` per pool, batched into a single simulate.
 * A batch is all-or-nothing, so a failure retries the pools one at a time to
 * isolate the bad pool instead of blanking twenty rows.
 */
async function fetchBooks(
  client: ReturnType<typeof useSuiGrpcClient>,
  viewer: string | null,
  markets: DeepBookMarket[],
): Promise<{ books: Record<string, RawBook>; errors: Record<string, string> }> {
  const call = (tx: Transaction, m: DeepBookMarket) => {
    tx.moveCall({
      target: `${DEEPBOOK_PACKAGE_ID}::pool::get_level2_ticks_from_mid`,
      typeArguments: [m.baseType, m.quoteType],
      arguments: [tx.object(m.poolId), tx.pure.u64(BOOK_TICKS), tx.object(SUI_CLOCK_OBJECT_ID)],
    });
  };
  const decode = (ret: Uint8Array[] | undefined): RawBook => {
    if (!ret || ret.length < 4) return { bids: [], asks: [] };
    const [bidPx, bidQty, askPx, askQty] = ret.map((bytes) => VEC_U64.parse(bytes));
    const levels = (px: string[], qty: string[]): RawLevel[] =>
      px.map((p, i) => ({ priceRaw: p, qtyRaw: qty[i] ?? "0" }));
    return { bids: levels(bidPx, bidQty), asks: levels(askPx, askQty) };
  };

  const books: Record<string, RawBook> = {};
  const errors: Record<string, string> = {};
  try {
    const results = await devInspect(client, viewer, (tx) => {
      for (const m of markets) call(tx, m);
    });
    markets.forEach((m, i) => {
      books[m.poolId] = decode(results[i]);
    });
  } catch {
    const single = await Promise.allSettled(
      markets.map((m) => devInspect(client, viewer, (tx) => call(tx, m))),
    );
    markets.forEach((m, i) => {
      const r = single[i];
      if (r.status === "fulfilled") books[m.poolId] = decode(r.value[0]);
      else errors[m.poolId] = r.reason instanceof Error ? r.reason.message : "read failed";
    });
  }
  return { books, errors };
}

export type BooksResult = {
  books: Record<string, RawBook>;
  errors: Record<string, string>;
  /** Batches that have returned at least once, out of the total. */
  loaded: number;
  batches: number;
  isFetching: boolean;
};

/** Live books for the given pools. Batches stream in independently. */
export function useOrderBooks(
  markets: DeepBookMarket[],
  viewer: string | null,
  refetchInterval: number,
): BooksResult {
  const client = useSuiGrpcClient();
  const batches = useMemo(() => {
    const out: DeepBookMarket[][] = [];
    for (let i = 0; i < markets.length; i += BOOK_BATCH) out.push(markets.slice(i, i + BOOK_BATCH));
    return out;
  }, [markets]);

  return useQueries({
    queries: batches.map((batch) => ({
      queryKey: ["deepbook-books-batch", batch.map((m) => m.poolId).join(",")],
      enabled: Boolean(DEEPBOOK_PACKAGE_ID) && batch.length > 0,
      refetchInterval,
      queryFn: () => fetchBooks(client, viewer, batch),
    })),
    combine: (results): BooksResult => {
      const books: Record<string, RawBook> = {};
      const errors: Record<string, string> = {};
      let loaded = 0;
      for (const r of results) {
        if (r.data) {
          loaded++;
          Object.assign(books, r.data.books);
          Object.assign(errors, r.data.errors);
        } else if (r.error) {
          loaded++;
        }
      }
      return {
        books,
        errors,
        loaded,
        batches: results.length,
        isFetching: results.some((r) => r.isFetching),
      };
    },
  });
}
