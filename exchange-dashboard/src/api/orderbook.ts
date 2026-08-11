// Typed client for the orderbook service (rust-backend/services/orderbook).
// Types hand-mirror the Rust DTOs — authorities:
//   Market:      rust-backend/crates/exchange-types/src/market.rs
//   SignedOrder: rust-backend/crates/exchange-types/src/order.rs
//   RoutePlan:   rust-backend/crates/exchange-router/src/lib.rs
// Wire quirk: SignedOrder u64s serialize as strings, RoutePlan u64s as JSON
// numbers — fields are `string | number` and callers convert via toBigint().

import { ORDERBOOK_URL } from "../config";

export type Market = {
  symbol: string;
  registryId: string;
  base: string; // canonical Move type string
  quote: string;
  tickSize: string | number;
  minSize: string | number;
  lotSize: string | number;
  currentFeeBps: string | number;
};

export type SignedOrder = {
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
 * One `ptbSkeleton` entry. Fill legs carry the command the server chose
 * for the maker's escrow binding (SO-372): plain settlement fills for
 * wallet makers, `fill_vault_order(_reverse)` + the exchange-adapter ids
 * for direct-escrow vault makers, whose identity BalanceManager holds
 * nothing — debiting it aborts, so those legs MUST go through the adapter.
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
  // fill_vault_order(_reverse) only:
  vaultId?: string;
  custodyId?: string;
  integrationRegistryId?: string;
  adapterPackageId?: string;
};

export type RouteResponse = {
  serverTimeMs: number;
  plan: RoutePlan;
  orders: Record<string, SignedOrder>;
  ptbSkeleton: SkeletonCommand[];
};

export class OrderbookApiError extends Error {
  constructor(
    public code: string,
    public detail: string,
    public status: number,
  ) {
    super(`${code}: ${detail}`);
  }
}

// RoutePlan u64s are bare JSON numbers; values at 2^53 and above would be
// silently rounded by JSON.parse. Quote 16+-digit integers first — every
// numeric field in these responses is `string | number`-tolerant.
function parseJsonSafe(text: string): unknown {
  return JSON.parse(text.replace(/([:[,]\s*)(\d{16,})(?=\s*[,}\]])/g, '$1"$2"'));
}

async function get<T>(path: string): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${ORDERBOOK_URL}${path}`);
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

export async function getMarkets(): Promise<Market[]> {
  const body = await get<{ markets: Market[] }>("/v1/markets");
  return body.markets;
}

export async function getRoutes(from: string, to: string, amount: bigint): Promise<RouteResponse> {
  const params = new URLSearchParams({ from, to, amount: amount.toString() });
  return get<RouteResponse>(`/v1/routes?${params}`);
}
