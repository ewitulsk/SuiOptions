// DeepBook read hooks (SO-157): BalanceManager discovery, order book,
// open orders, BM balances. All chain reads go through devInspect — no
// gas, no signatures — and return values decode with @mysten/sui's bcs.
//
// BM discovery: our "enable trading" PTB registers the BalanceManager, which
// emits `BalanceManagerEvent { balance_manager_id, owner }` (the only
// creation-adjacent event on the deployed package — see
// DEEPBOOK-FINDINGS.md §D). localStorage is just a cache over that query, so
// a fresh browser profile recovers the same BM.

import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";
import { bcs } from "@mysten/sui/bcs";
import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID, normalizeStructTag, normalizeSuiAddress } from "@mysten/sui/utils";

import { DEEPBOOK_ORIGINAL_PACKAGE_ID, DEEPBOOK_PACKAGE_ID } from "../config";
import { fromRawPrice } from "../tx/deepbook";
import type { Bucket, Series } from "./client";

const BM_CACHE_PREFIX = "tideline-bm-";
const BOOK_TICKS = 8n;

/** dapp-kit's client type, inferred so SDK reshuffles can't break the import. */
type SuiClient = ReturnType<typeof useSuiClient>;

/** Pool identity the read hooks need. */
export type PoolRef = {
  poolId: string;
  baseCoinType: string;
  quoteCoinType: string;
  baseDecimals: number;
  quoteDecimals: number;
};

// ---- devInspect plumbing ----------------------------------------------------

async function devInspect(
  client: SuiClient,
  sender: string | null,
  build: (tx: Transaction) => void,
): Promise<Uint8Array[][]> {
  const tx = new Transaction();
  build(tx);
  const res = await client.devInspectTransactionBlock({
    sender: sender ?? normalizeSuiAddress("0x0"),
    transactionBlock: tx,
  });
  if (res.error) throw new Error(`devInspect failed: ${res.error}`);
  type ExecResult = { returnValues?: Array<[number[], string]> };
  return ((res.results ?? []) as ExecResult[]).map((r) =>
    (r.returnValues ?? []).map(([bytes]) => new Uint8Array(bytes)),
  );
}

const VEC_U64 = bcs.vector(bcs.u64());
const VEC_U128 = bcs.vector(bcs.u128());

// ---- BalanceManager discovery ------------------------------------------------

export async function findBalanceManager(
  client: SuiClient,
  owner: string,
): Promise<string | null> {
  const orig = DEEPBOOK_ORIGINAL_PACKAGE_ID;
  if (!orig) return null;

  const cached = localStorage.getItem(BM_CACHE_PREFIX + owner);
  if (cached) return cached;

  const want = normalizeSuiAddress(owner);
  let cursor: { txDigest: string; eventSeq: string } | null | undefined;
  // BalanceManagerEvent only fires on register — a handful exist, so a few
  // pages cover the whole stream.
  for (let page = 0; page < 5; page++) {
    const res = await client.queryEvents({
      query: { MoveEventType: `${orig}::balance_manager::BalanceManagerEvent` },
      cursor,
      limit: 50,
      order: "descending",
    });
    for (const ev of res.data) {
      const json = ev.parsedJson as { owner?: string; balance_manager_id?: string };
      if (json.owner && normalizeSuiAddress(json.owner) === want && json.balance_manager_id) {
        localStorage.setItem(BM_CACHE_PREFIX + owner, json.balance_manager_id);
        return json.balance_manager_id;
      }
    }
    if (!res.hasNextPage || !res.nextCursor) break;
    cursor = res.nextCursor;
  }
  return null;
}

export function cacheBalanceManager(owner: string, bmId: string) {
  localStorage.setItem(BM_CACHE_PREFIX + owner, bmId);
}

/** The connected wallet's BalanceManager id, or null until one is created. */
export function useBalanceManager(owner: string | null) {
  const client = useSuiClient();
  return useQuery<string | null, Error>({
    queryKey: ["deepbook-bm", owner],
    enabled: owner !== null && Boolean(DEEPBOOK_ORIGINAL_PACKAGE_ID),
    refetchInterval: 10_000,
    queryFn: () => (owner ? findBalanceManager(client, owner) : null),
  });
}

// ---- order book ---------------------------------------------------------------

export type BookLevel = { price: number; qty: number };
export type OrderBook = { bids: BookLevel[]; asks: BookLevel[] };

/**
 * The `PoolRef` for a bucket's DeepBook venue, or `null` if it has no pool.
 * One place to map a (`bucket`, `series`) pair to the read-hook identity so
 * `Orderbook`, the Composer, and the metrics panel all derive the same pool.
 */
export function poolRefFor(bucket: Bucket, series: Series): PoolRef | null {
  if (!bucket.deepbook_pool_id) return null;
  return {
    poolId: bucket.deepbook_pool_id,
    baseCoinType: bucket.call_coin_type,
    quoteCoinType: series.settlement_coin_type,
    baseDecimals: series.asset_decimals ?? 8,
    quoteDecimals: series.settlement_decimals ?? 6,
  };
}

/**
 * Top-of-book mid = (best bid + best ask) / 2 in quote (settlement) units.
 * `null` when either side is empty — a one-sided book has no mid, and the
 * metrics endpoint needs a real two-sided mark.
 */
export function midFromBook(book: OrderBook | undefined): number | null {
  const bid = book?.bids[0]?.price;
  const ask = book?.asks[0]?.price;
  if (bid == null || ask == null || !(bid > 0) || !(ask > 0)) return null;
  return (bid + ask) / 2;
}

/**
 * Top-of-book via `get_level2_ticks_from_mid`, which returns four u64
 * vectors: bid prices, bid quantities, ask prices, ask quantities. Converted
 * to display units (prices best-first in both lists).
 */
export function useOrderBook(
  pool: PoolRef | null,
  viewer: string | null,
  // Chain-table rows fetch many pools at once; let callers slow their poll so a
  // wide chain doesn't fan out a devInspect per strike every 3s (SO-225).
  refetchInterval = 3_000,
) {
  const client = useSuiClient();
  return useQuery<OrderBook, Error>({
    queryKey: ["deepbook-book", pool?.poolId],
    enabled: Boolean(pool && DEEPBOOK_PACKAGE_ID),
    refetchInterval,
    queryFn: async () => {
      const p = pool!;
      const results = await devInspect(client, viewer, (tx) => {
        tx.moveCall({
          target: `${DEEPBOOK_PACKAGE_ID}::pool::get_level2_ticks_from_mid`,
          typeArguments: [p.baseCoinType, p.quoteCoinType],
          arguments: [tx.object(p.poolId), tx.pure.u64(BOOK_TICKS), tx.object(SUI_CLOCK_OBJECT_ID)],
        });
      });
      const ret = results[0] ?? [];
      if (ret.length < 4) return { bids: [], asks: [] };
      const [bidPx, bidQty, askPx, askQty] = ret.map((bytes) => VEC_U64.parse(bytes));
      const level = (px: string, qty: string): BookLevel => ({
        price: fromRawPrice(BigInt(px), p.baseDecimals, p.quoteDecimals),
        qty: Number(qty) / 10 ** p.baseDecimals,
      });
      return {
        bids: bidPx.map((px, i) => level(px, bidQty[i] ?? "0")),
        asks: askPx.map((px, i) => level(px, askQty[i] ?? "0")),
      };
    },
  });
}

// ---- open orders ---------------------------------------------------------------

/** The BM's open order ids on one pool (`account_open_orders` → VecSet<u128>). */
export function useOpenOrders(pool: PoolRef | null, bmId: string | null, viewer: string | null) {
  const client = useSuiClient();
  return useQuery<string[], Error>({
    queryKey: ["deepbook-open-orders", pool?.poolId, bmId],
    enabled: Boolean(pool && bmId && DEEPBOOK_PACKAGE_ID),
    refetchInterval: 4_000,
    queryFn: async () => {
      const p = pool!;
      const results = await devInspect(client, viewer, (tx) => {
        tx.moveCall({
          target: `${DEEPBOOK_PACKAGE_ID}::pool::account_open_orders`,
          typeArguments: [p.baseCoinType, p.quoteCoinType],
          arguments: [tx.object(p.poolId), tx.object(bmId!)],
        });
      });
      const bytes = results[0]?.[0];
      if (!bytes) return [];
      // VecSet<u128> BCS-encodes as its inner vector.
      return VEC_U128.parse(bytes);
    },
  });
}

/**
 * One open order with the fields a user actually reads: side, price, and the
 * still-resting (unfilled) base quantity.
 *
 * `account_open_orders` only hands back the encoded order ids, so we decode
 * side+price straight from the u128 (DeepBook packs them into the high bits —
 * see utils::decode_order_id) and devInspect `get_order` per id for the
 * filled/total quantity the id doesn't carry.
 */
export type OpenOrder = {
  orderId: string;
  isBid: boolean;
  price: number; // quote (settlement) display units per option
  remaining: number; // unfilled base quantity, display units
};

// BCS shape of deepbook `book::order::Order` (returned by `pool::get_order`).
const ORDER_BCS = bcs.struct("Order", {
  balance_manager_id: bcs.Address,
  order_id: bcs.u128(),
  client_order_id: bcs.u64(),
  quantity: bcs.u64(),
  filled_quantity: bcs.u64(),
  fee_is_deep: bcs.bool(),
  order_deep_price: bcs.struct("OrderDeepPrice", {
    asset_is_base: bcs.bool(),
    deep_per_asset: bcs.u64(),
  }),
  epoch: bcs.u64(),
  status: bcs.u8(),
  expire_timestamp: bcs.u64(),
});

/** Split a DeepBook encoded order id into (is_bid, raw price). */
function decodeOrderId(id: bigint): { isBid: boolean; priceRaw: bigint } {
  return {
    isBid: id >> 127n === 0n,
    priceRaw: (id >> 64n) & ((1n << 63n) - 1n),
  };
}

/**
 * Resolve each open-order id to its side / price / remaining size. Batches one
 * `get_order` devInspect call per id into a single transaction. Keyed on the id
 * list so it refetches whenever `useOpenOrders` reports a change.
 */
export function useOpenOrderDetails(
  pool: PoolRef | null,
  orderIds: string[] | undefined,
  viewer: string | null,
) {
  const client = useSuiClient();
  const ids = orderIds ?? [];
  return useQuery<OpenOrder[], Error>({
    queryKey: ["deepbook-open-order-details", pool?.poolId, ids.join(",")],
    enabled: Boolean(pool && DEEPBOOK_PACKAGE_ID) && ids.length > 0,
    refetchInterval: 4_000,
    queryFn: async () => {
      const p = pool!;
      const results = await devInspect(client, viewer, (tx) => {
        for (const id of ids) {
          tx.moveCall({
            target: `${DEEPBOOK_PACKAGE_ID}::pool::get_order`,
            typeArguments: [p.baseCoinType, p.quoteCoinType],
            arguments: [tx.object(p.poolId), tx.pure.u128(BigInt(id))],
          });
        }
      });
      return ids.map((id, i) => {
        const { isBid, priceRaw } = decodeOrderId(BigInt(id));
        const price = fromRawPrice(priceRaw, p.baseDecimals, p.quoteDecimals);
        const bytes = results[i]?.[0];
        if (!bytes) return { orderId: id, isBid, price, remaining: 0 };
        const o = ORDER_BCS.parse(bytes);
        const remaining = (Number(o.quantity) - Number(o.filled_quantity)) / 10 ** p.baseDecimals;
        return { orderId: id, isBid, price, remaining };
      });
    },
  });
}

// ---- BM balances ----------------------------------------------------------------

export type BmBalances = { baseRaw: bigint; quoteRaw: bigint };

/** The BM's available (unlocked) balances of the pool's two assets. */
export function useBmBalances(pool: PoolRef | null, bmId: string | null, viewer: string | null) {
  const client = useSuiClient();
  return useQuery<BmBalances, Error>({
    queryKey: ["deepbook-bm-balances", pool?.poolId, bmId],
    enabled: Boolean(pool && bmId && DEEPBOOK_PACKAGE_ID),
    refetchInterval: 4_000,
    queryFn: async () => {
      const p = pool!;
      const results = await devInspect(client, viewer, (tx) => {
        for (const t of [p.baseCoinType, p.quoteCoinType]) {
          tx.moveCall({
            target: `${DEEPBOOK_PACKAGE_ID}::balance_manager::balance`,
            typeArguments: [t],
            arguments: [tx.object(bmId!)],
          });
        }
      });
      const parse = (i: number) => {
        const bytes = results[i]?.[0];
        return bytes ? BigInt(bcs.u64().parse(bytes)) : 0n;
      };
      return { baseRaw: parse(0), quoteRaw: parse(1) };
    },
  });
}

/**
 * The BM's available balance for an arbitrary set of coin types, keyed by the
 * normalized type. One devInspect calls `balance_manager::balance<T>` per type.
 * Used by the dashboard to surface position tokens (and settlement) the user
 * left sitting in their trading account rather than their wallet.
 */
export function useBmCoinBalances(
  bmId: string | null,
  coinTypes: string[],
  viewer: string | null,
) {
  const client = useSuiClient();
  // Stable, de-duplicated type list so the query key is order-independent.
  const types = Array.from(new Set(coinTypes.map((t) => normalizeStructTag(t)))).sort();
  return useQuery<Record<string, bigint>, Error>({
    queryKey: ["deepbook-bm-coin-balances", bmId, types.join(",")],
    enabled: Boolean(bmId && types.length > 0 && DEEPBOOK_PACKAGE_ID),
    refetchInterval: 5_000,
    queryFn: async () => {
      const results = await devInspect(client, viewer, (tx) => {
        for (const t of types) {
          tx.moveCall({
            target: `${DEEPBOOK_PACKAGE_ID}::balance_manager::balance`,
            typeArguments: [t],
            arguments: [tx.object(bmId!)],
          });
        }
      });
      const out: Record<string, bigint> = {};
      types.forEach((t, i) => {
        const bytes = results[i]?.[0];
        out[t] = bytes ? BigInt(bcs.u64().parse(bytes)) : 0n;
      });
      return out;
    },
  });
}
