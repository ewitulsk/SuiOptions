// Thin HTTP client for the Rust `api-service` backend.
//
// Base URL defaults to local dev. Override with `VITE_API_BASE_URL` in
// `.env.local` (e.g. `VITE_API_BASE_URL=https://api.staging.example.com`).
//
// Authoritative spec for the response shape lives next to the handler:
//   rust-backend/services/api-service/README.md
//   rust-backend/services/api-service/src/handlers/buckets.rs (doc comment)
// Keep this file in sync with `BucketDto` / `SeriesDto` / `BucketsResponse`.

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

/**
 * One option-writing bucket within a series.
 *
 * Numeric fields ship in two flavors:
 * - **scaled** (`number`) — divided by the relevant token's decimals,
 *   display-ready. `null` if decimals are unknown for the coin type.
 * - **raw** (`string`) — on-chain integer in atomic units, kept as a
 *   string to preserve u64/u128 precision. Use these when building a tx.
 */
export type Bucket = {
  bucket_id: string;
  strike: number | null;
  strike_raw: string;
  /** On-chain `strike_scale`. Real ratio = `strike_raw / 10^strike_scale`. */
  strike_scale: number;
  total_written: number | null;
  total_written_raw: string;
  exercise_cursor: number | null;
  exercise_cursor_raw: string;
  /** `100 * exercise_cursor / total_written`; `0` when nothing written; `null` if decimals unknown. */
  fill_pct: number | null;
  /**
   * Admin freeze on new writes. Both flows of `execute_write` revert
   * against an invalidated bucket; the writer screen filters these out
   * entirely. Exercises and redeems are unaffected. See SO-69.
   */
  invalidated: boolean;
};

/**
 * A series is the set of buckets that share `(asset, settlement, expiry)`.
 * Strikes within a series are the user-facing selection axis.
 */
export type Series = {
  /** Friendly symbol (e.g. `"TBTC"`) or raw Move type if unknown. */
  asset_symbol: string;
  asset_decimals: number | null;
  /** Full Move coin type — the `Underlying` type arg for PTB builders. */
  asset_coin_type: string;
  settlement_symbol: string;
  settlement_decimals: number | null;
  settlement_coin_type: string;
  /** Unix millis. Safe to use directly with `new Date(...)`. */
  expiry_ms: number;
  /** Pre-formatted ISO-8601 UTC, e.g. `"2026-06-26T08:00:00Z"`. */
  expiry_iso: string;
  /** Sorted ascending by `strike`. */
  buckets: Bucket[];
};

export type BucketsResponse = {
  series: Series[];
};

export async function fetchBuckets(): Promise<Series[]> {
  const res = await fetch(`${API_BASE_URL}/buckets`);
  if (!res.ok) {
    throw new Error(`GET /buckets failed: ${res.status} ${res.statusText}`);
  }
  const body: BucketsResponse = await res.json();
  return body.series;
}

/**
 * One `Position` object owned by the caller's wallet. Mirrors
 * `api-service::handlers::positions::PositionDto`. Raw u128 fields ship
 * as decimal strings; the frontend divides by `asset_decimals` for
 * display.
 */
export type Position = {
  position_object_id: string;
  bucket_id: string;
  asset_symbol: string;
  asset_decimals: number | null;
  asset_coin_type: string;
  settlement_symbol: string;
  settlement_decimals: number | null;
  settlement_coin_type: string;
  strike: number | null;
  strike_raw: string;
  strike_scale: number;
  expiry_ms: number;
  range_start_raw: string;
  range_end_raw: string;
  total_written_raw: string;
  exercise_cursor_raw: string;
  premium_received_raw: string;
  mm_account_id: string;
  minted_at_ms: number;
};

export type PositionsResponse = { positions: Position[] };

export async function fetchPositions(wallet: string): Promise<Position[]> {
  const res = await fetch(
    `${API_BASE_URL}/positions?wallet=${encodeURIComponent(wallet)}`,
  );
  if (!res.ok) {
    throw new Error(`GET /positions failed: ${res.status} ${res.statusText}`);
  }
  const body: PositionsResponse = await res.json();
  return body.positions;
}

/**
 * One `WriteExecuted` event where the caller's wallet was the
 * `call_token_recipient`. Used to show provenance for owned-call cards
 * (`boughtFrom`, `premiumPaid`, `boughtAt`). Mirrors
 * `api-service::handlers::call_token_lots::LotDto`.
 */
export type CallTokenLot = {
  call_option_id: string;
  bucket_id: string;
  asset_symbol: string;
  asset_decimals: number | null;
  asset_coin_type: string;
  settlement_symbol: string;
  settlement_decimals: number | null;
  settlement_coin_type: string;
  strike: number | null;
  strike_raw: string;
  strike_scale: number;
  expiry_ms: number;
  amount_raw: string;
  premium_paid_raw: string;
  seller_account_id: string;
  timestamp_ms: number;
};

export type CallTokenLotsResponse = { lots: CallTokenLot[] };

export async function fetchCallTokenLots(wallet: string): Promise<CallTokenLot[]> {
  const res = await fetch(
    `${API_BASE_URL}/call-token-lots?wallet=${encodeURIComponent(wallet)}`,
  );
  if (!res.ok) {
    throw new Error(
      `GET /call-token-lots failed: ${res.status} ${res.statusText}`,
    );
  }
  const body: CallTokenLotsResponse = await res.json();
  return body.lots;
}

/**
 * Enriched written position (SO-97). The frontend reads the authoritative
 * id list from the wallet, then posts the ids here; api-service joins each to
 * the indexer's bucket + provenance. Mirrors
 * `api-service::handlers::dashboard::EnrichedPositionDto`. Ids the indexer
 * doesn't know yet are simply absent from the response.
 */
export type EnrichedPosition = {
  position_object_id: string;
  bucket_id: string;
  asset_symbol: string;
  asset_decimals: number | null;
  asset_coin_type: string;
  settlement_symbol: string;
  settlement_decimals: number | null;
  settlement_coin_type: string;
  strike: number | null;
  strike_raw: string;
  strike_scale: number;
  expiry_ms: number;
  range_start_raw: string;
  range_end_raw: string;
  total_written_raw: string;
  exercise_cursor_raw: string;
  premium_received_raw: string;
  mm_account_id: string;
  /** Minting tx digest for explorer links; empty string if unknown. */
  tx_digest: string;
  minted_at_ms: number;
};

export type EnrichResponse = { positions: EnrichedPosition[] };

export async function fetchEnrichedPositions(
  objectIds: string[],
): Promise<EnrichedPosition[]> {
  if (objectIds.length === 0) return [];
  const res = await fetch(`${API_BASE_URL}/dashboard/positions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ object_ids: objectIds }),
  });
  if (!res.ok) {
    throw new Error(
      `POST /dashboard/positions failed: ${res.status} ${res.statusText}`,
    );
  }
  const body: EnrichResponse = await res.json();
  return body.positions;
}
