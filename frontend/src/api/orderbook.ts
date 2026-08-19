// Typed client + React hooks for the in-house exchange orderbook service
// (SO-416; rust-backend/services/orderbook). Types hand-mirror the Rust DTOs:
//   Market:      rust-backend/crates/exchange-types/src/market.rs
//   SignedOrder: rust-backend/crates/exchange-types/src/order.rs
//   BookLevel:   rust-backend/crates/exchange-book/src/lib.rs
//   RoutePlan:   rust-backend/crates/exchange-router/src/lib.rs
// Wire quirk: SignedOrder u64s serialize as strings, RoutePlan/book u64s as
// JSON numbers — fields are `string | number`-tolerant and 16+-digit
// integers get quoted before JSON.parse so nothing rounds at 2^53.

import { useQuery } from "@tanstack/react-query";
import { normalizeStructTag } from "@mysten/sui/utils";

import { ORDERBOOK_URL } from "../config";
import { optionCoinType, type Bucket } from "./client";

// ---- wire types -------------------------------------------------------------

export type ExchangeMarket = {
  symbol: string;
  registryId: string;
  base: string; // canonical Move type string
  quote: string;
  /** Quote units per `lotSize` base units, per price tick. */
  tickSize: string | number;
  minSize: string | number;
  lotSize: string | number;
  currentFeeBps: string | number;
};

/** Shared ids takers need for direct-escrow vault-maker fill legs (SO-372). */
export type DirectEscrowInfo = {
  adapterPackageId: string;
  integrationRegistryId: string;
};

export type MarketsInfo = {
  packageId: string;
  /** Shared ingress `Whitelist` every fill entry takes (SO-384). */
  whitelistId: string | null;
  directEscrow: DirectEscrowInfo | null;
  markets: ExchangeMarket[];
};

/** Signed maker order as wired over `POST /v1/orders` / route responses. */
export type SignedOrderWire = {
  makerToken: string;
  takerToken: string;
  makerAmount: string | number;
  takerAmount: string | number;
  maxFeeBps: string | number;
  maker: string;
  makerManagerId: string;
  taker: string;
  sender: string;
  expiryMs: number;
  salt: string | number;
  registryId: string;
  scheme: "ed25519" | "secp256k1";
  signature: string; // base64, raw 64 bytes (no scheme flag)
  publicKey: string; // base64
};

export type WireBookLevel = {
  priceTicks: string | number;
  baseQuantity: string | number;
  orderCount: string | number;
};

export type BookResponse = {
  market: string;
  tickSize: string | number;
  lotSize: string | number;
  bids: WireBookLevel[];
  asks: WireBookLevel[];
};

export type OrderStatus = "OPEN" | "FILLED" | "CANCELLED" | "PRUNED" | "EXPIRED";

export type AccountOrder = {
  digest: string;
  order: SignedOrderWire;
  status: OrderStatus | string;
  filledTaker: string | number;
};

export type AccountFill = {
  txDigest: string;
  eventSeq: number;
  digest: string;
  registryId: string;
  maker: string;
  taker: string;
  baseAmount: string | number;
  quoteAmount: string | number;
  makerFee: string | number;
  takerFee: string | number;
  makerSoldBase: boolean;
  filledTotal: string | number;
  timestampMs: string | number;
};

export type EscrowBalance = { token: string; amount: string | number };

export type FillLeg = { digest: string; amountIn: string | number };

export type PathPlan = {
  tokens: string[]; // token chain [from, mid…, to]
  markets: string[]; // registry ids, one per hop
  input: string | number;
  expectedOut: string | number;
  hops: FillLeg[][];
};

export type RoutePlan = {
  input: string | number;
  unrouted: string | number;
  expectedOut: string | number;
  paths: PathPlan[];
};

/**
 * One `ptbSkeleton` entry. Fill legs carry the command the server chose for
 * the maker's escrow binding (SO-372): plain settlement fills for wallet
 * makers, `fill_vault_order(_reverse)` + the exchange-adapter ids for
 * direct-escrow vault makers.
 */
export type SkeletonCommand = {
  command:
    | "fill_limit_order"
    | "fill_limit_order_reverse"
    | "fill_vault_order"
    | "fill_vault_order_reverse"
    | "assert_coin_min";
  digest?: string;
  market?: string;
  typeArgs?: string[];
  amountIn?: string | number;
  minMakerAmountOut?: string | number;
  min?: string | number;
  /** Shared ingress Whitelist (SO-384); null on pre-whitelist deployments. */
  whitelistId?: string | null;
  // fill_vault_order(_reverse) only:
  vaultId?: string;
  custodyId?: string;
  integrationRegistryId?: string;
  adapterPackageId?: string;
};

export type RouteResponse = {
  serverTimeMs: number;
  plan: RoutePlan;
  orders: Record<string, SignedOrderWire>;
  ptbSkeleton: SkeletonCommand[];
};

export type PlaceOrderResponse = {
  serverTimeMs: number;
  digest: string;
  status: "MATCHED" | "OPEN" | "SELF_TRADE_CANCELLED" | string;
  matches: number;
};

export type CancelOrderRequest = {
  scheme: "ed25519" | "secp256k1";
  /** base64 raw 64-byte signature over the cancel personal message. */
  signature: string;
  /** base64 public key. */
  publicKey: string;
};

// ---- fetch plumbing ---------------------------------------------------------

export class OrderbookApiError extends Error {
  constructor(
    public code: string,
    public detail: string,
    public status: number,
  ) {
    super(`${code}: ${detail}`);
  }
}

export function toBigint(v: string | number): bigint {
  return typeof v === "bigint" ? v : BigInt(v);
}

// RoutePlan/book u64s are bare JSON numbers; values at 2^53 and above would
// be silently rounded by JSON.parse. Quote 16+-digit integers first — every
// numeric field in these responses is `string | number`-tolerant.
function parseJsonSafe(text: string): unknown {
  return JSON.parse(text.replace(/([:[,]\s*)(\d{16,})(?=\s*[,}\]])/g, '$1"$2"'));
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const base = ORDERBOOK_URL.replace(/\/$/, "");
  let res: Response;
  try {
    res = await fetch(`${base}${path}`, init);
  } catch (err) {
    throw new OrderbookApiError("UNREACHABLE", `orderbook service unreachable: ${err}`, 0);
  }
  const text = await res.text();
  if (!res.ok) {
    let code = `HTTP_${res.status}`;
    let detail = text;
    try {
      const body = JSON.parse(text) as { error?: { code?: string; detail?: string } };
      if (body.error?.code) {
        code = body.error.code;
        detail = body.error.detail ?? "";
      }
    } catch {
      // non-JSON error body; keep raw text
    }
    throw new OrderbookApiError(code, detail, res.status);
  }
  return parseJsonSafe(text) as T;
}

const jsonInit = (method: string, body: unknown): RequestInit => ({
  method,
  headers: { "content-type": "application/json" },
  body: JSON.stringify(body),
});

// ---- REST calls -------------------------------------------------------------

export async function getMarketsInfo(): Promise<MarketsInfo> {
  const body = await request<{
    packageId: string;
    whitelistId?: string | null;
    directEscrow?: DirectEscrowInfo | null;
    markets: ExchangeMarket[];
  }>("/v1/markets");
  return {
    packageId: body.packageId,
    whitelistId: body.whitelistId ?? null,
    directEscrow: body.directEscrow ?? null,
    markets: body.markets,
  };
}

export async function getBook(marketId: string, depth = 20): Promise<BookResponse> {
  return request<BookResponse>(`/v1/markets/${marketId}/book?depth=${depth}`);
}

export async function getRoutes(from: string, to: string, amount: bigint): Promise<RouteResponse> {
  const params = new URLSearchParams({ from, to, amount: amount.toString() });
  return request<RouteResponse>(`/v1/routes?${params}`);
}

export async function postOrder(order: SignedOrderWire): Promise<PlaceOrderResponse> {
  return request<PlaceOrderResponse>("/v1/orders", jsonInit("POST", order));
}

export async function cancelOrder(
  digest: string,
  req: CancelOrderRequest,
): Promise<{ digest: string; status: string; stillFillableOnChain?: boolean }> {
  return request(`/v1/orders/${digest}`, jsonInit("DELETE", req));
}

export async function getAccountOrders(addr: string): Promise<AccountOrder[]> {
  const body = await request<{ orders: AccountOrder[] }>(`/v1/accounts/${addr}/orders`);
  return body.orders;
}

export async function getAccountFills(addr: string): Promise<AccountFill[]> {
  const body = await request<{ fills: AccountFill[] }>(`/v1/accounts/${addr}/fills`);
  return body.fills;
}

/** Escrow balances are keyed by MANAGER id (the BalanceManager object). */
export async function getEscrowBalances(managerId: string): Promise<EscrowBalance[]> {
  const body = await request<{ balances: EscrowBalance[] }>(
    `/v1/accounts/${managerId}/balance`,
  );
  return body.balances;
}

// ---- unit conversions -------------------------------------------------------
//
// price_ticks = quote_amount × lot_size / (base_amount × tick_size), so the
// display price (quote display units per base display unit) is
//   ticks × tickSize / lotSize × 10^(baseDec − quoteDec).

export type BookLevel = { price: number; qty: number };
export type OrderBook = { bids: BookLevel[]; asks: BookLevel[] };

export function priceFromTicks(
  ticks: bigint,
  market: ExchangeMarket,
  baseDecimals: number,
  quoteDecimals: number,
): number {
  const tick = Number(toBigint(market.tickSize));
  const lot = Number(toBigint(market.lotSize));
  if (!(lot > 0)) return 0;
  return ((Number(ticks) * tick) / lot) * 10 ** (baseDecimals - quoteDecimals);
}

/** Snap a display price onto the market's tick grid ("bid" rounds down,
 * "ask" rounds up — the direction that can only improve the order for its
 * owner). Returns the tick count. */
export function ticksFromPrice(
  price: number,
  market: ExchangeMarket,
  baseDecimals: number,
  quoteDecimals: number,
  mode: "bid" | "ask",
): bigint {
  const tick = Number(toBigint(market.tickSize));
  const lot = Number(toBigint(market.lotSize));
  if (!(tick > 0) || !(price > 0) || !Number.isFinite(price)) return 0n;
  const raw = (price * 10 ** (quoteDecimals - baseDecimals) * lot) / tick;
  const snapped = mode === "bid" ? Math.floor(raw) : Math.ceil(raw);
  return BigInt(Math.max(0, snapped));
}

/** Base atomic amount floored onto the market's lot grid. */
export function snapToLot(baseRaw: bigint, market: ExchangeMarket): bigint {
  const lot = toBigint(market.lotSize);
  if (lot <= 0n) return baseRaw;
  return (baseRaw / lot) * lot;
}

/** Book wire levels → display units (prices best-first in both lists, as
 * served). */
export function toDisplayBook(
  book: BookResponse | undefined,
  market: ExchangeMarket | null,
  baseDecimals: number,
  quoteDecimals: number,
): OrderBook | undefined {
  if (!book || !market) return undefined;
  const level = (l: WireBookLevel): BookLevel => ({
    price: priceFromTicks(toBigint(l.priceTicks), market, baseDecimals, quoteDecimals),
    qty: Number(toBigint(l.baseQuantity)) / 10 ** baseDecimals,
  });
  return { bids: book.bids.map(level), asks: book.asks.map(level) };
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

// ---- hooks ------------------------------------------------------------------

/**
 * The exchange deployment + market list. Polled (not `staleTime: Infinity`)
 * because option markets are listed permissionlessly at runtime (SO-415) and
 * a just-listed registry must show up without a reload.
 */
export function useExchangeMarkets(refetchInterval = 10_000) {
  return useQuery<MarketsInfo, Error>({
    queryKey: ["exchange-markets"],
    refetchInterval,
    queryFn: getMarketsInfo,
  });
}

/**
 * Resolve a bucket's exchange market. Prefers the api-service's
 * `exchange_market_id` (SO-416); falls back to matching the bucket's option
 * coin type against the market list so the UI works before the backend
 * serves the field. `market` is null while loading or when the bucket has
 * no listed market yet.
 */
export function useExchangeMarketFor(bucket: Bucket | null) {
  const info = useExchangeMarkets();
  const markets = info.data?.markets ?? [];
  let market: ExchangeMarket | null = null;
  if (bucket) {
    if (bucket.exchange_market_id) {
      market = markets.find((m) => m.registryId === bucket.exchange_market_id) ?? null;
    }
    if (!market) {
      const base = normalizeStructTag(optionCoinType(bucket));
      market = markets.find((m) => normalizeStructTag(m.base) === base) ?? null;
    }
  }
  return { market, info: info.data ?? null, isLoading: info.isLoading };
}

/** Order book for one market, in raw wire units. 3s poll by default; the
 * chain table slows this down per-row (SO-225). */
export function useExchangeBook(marketId: string | null, depth = 20, refetchInterval = 3_000) {
  return useQuery<BookResponse, Error>({
    queryKey: ["exchange-book", marketId, depth],
    enabled: marketId !== null,
    refetchInterval,
    queryFn: () => getBook(marketId!, depth),
  });
}

/** The maker's orders (all markets, newest-first). Filter by registry at the
 * call site. */
export function useOpenExchangeOrders(addr: string | null) {
  return useQuery<AccountOrder[], Error>({
    queryKey: ["exchange-open-orders", addr],
    enabled: addr !== null,
    refetchInterval: 4_000,
    queryFn: () => getAccountOrders(addr!),
  });
}

/** Chain-confirmed fills where `addr` was maker or taker. */
export function useAccountFills(addr: string | null) {
  return useQuery<AccountFill[], Error>({
    queryKey: ["exchange-account-fills", addr],
    enabled: addr !== null,
    refetchInterval: 5_000,
    queryFn: () => getAccountFills(addr!),
  });
}

/** Escrow balances of one exchange BalanceManager, keyed by canonical coin
 * type. */
export function useEscrowBalances(managerId: string | null) {
  return useQuery<Record<string, bigint>, Error>({
    queryKey: ["exchange-escrow-balances", managerId],
    enabled: managerId !== null,
    refetchInterval: 4_000,
    queryFn: async () => {
      const balances = await getEscrowBalances(managerId!);
      const out: Record<string, bigint> = {};
      for (const b of balances) {
        out[normalizeStructTag(b.token)] = toBigint(b.amount);
      }
      return out;
    },
  });
}
