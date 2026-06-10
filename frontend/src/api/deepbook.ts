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
import { SUI_CLOCK_OBJECT_ID, normalizeSuiAddress } from "@mysten/sui/utils";

import { DEEPBOOK_ORIGINAL_PACKAGE_ID, DEEPBOOK_PACKAGE_ID } from "../config";
import { fromRawPrice } from "../tx/deepbook";

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
 * Top-of-book via `get_level2_ticks_from_mid`, which returns four u64
 * vectors: bid prices, bid quantities, ask prices, ask quantities. Converted
 * to display units (prices best-first in both lists).
 */
export function useOrderBook(pool: PoolRef | null, viewer: string | null) {
  const client = useSuiClient();
  return useQuery<OrderBook, Error>({
    queryKey: ["deepbook-book", pool?.poolId],
    enabled: Boolean(pool && DEEPBOOK_PACKAGE_ID),
    refetchInterval: 3_000,
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
