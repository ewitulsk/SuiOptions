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
  total_written: number | null;
  total_written_raw: string;
  exercise_cursor: number | null;
  exercise_cursor_raw: string;
  /** `100 * exercise_cursor / total_written`; `0` when nothing written; `null` if decimals unknown. */
  fill_pct: number | null;
};

/**
 * A series is the set of buckets that share `(asset, settlement, expiry)`.
 * Strikes within a series are the user-facing selection axis.
 */
export type Series = {
  /** Friendly symbol (e.g. `"TBTC"`) or raw Move type if unknown. */
  asset_symbol: string;
  asset_decimals: number | null;
  settlement_symbol: string;
  settlement_decimals: number | null;
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
