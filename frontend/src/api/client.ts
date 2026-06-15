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
  /**
   * Fully-qualified type of this bucket's fungible option coin
   * (`Coin<call_coin_type>`). Used as the `Call` type arg when exercising
   * and to match the wallet's owned option coins back to their bucket.
   */
  call_coin_type: string;
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
  /**
   * DeepBook pool trading this bucket's call coin against the settlement
   * asset (SO-153). `null` until someone creates the venue.
   */
  deepbook_pool_id: string | null;
  /**
   * Pool exists, bucket not cleaned, not expired. Gates the DeepBook trade
   * UI; `invalidated` does NOT affect it (mint freeze only).
   */
  tradeable: boolean;
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

// ── dashboard PnL (SO-209) — FIFO cost-basis lots + realized PnL ──────────
//
// Mirrors `api-service::handlers::pnl`. Amounts are display units (underlying
// tokens / settlement USD), so the dashboard renders them directly. `source`
// is `rfq`|`deepbook` from the backend; the frontend appends `transfer` rows
// when reconciling against current holdings.

export type PnlLotSource = "rfq" | "deepbook" | "transfer";

export type PnlLot = {
  amount: number;
  cost: number;
  source: PnlLotSource;
  acquired_at_ms: number;
};

export type BucketPnl = {
  bucket_id: string;
  asset_decimals: number;
  settlement_decimals: number;
  remaining_lots: PnlLot[];
  realized_pnl: number;
  unpriced_exercise_amount: number;
};

export type DashboardPnlResponse = { buckets: BucketPnl[] };

export async function fetchDashboardPnl(
  wallet: string,
  bm: string | null,
): Promise<BucketPnl[]> {
  const bmParam = bm ? `&bm=${encodeURIComponent(bm)}` : "";
  const res = await fetch(
    `${API_BASE_URL}/dashboard/pnl?wallet=${encodeURIComponent(wallet)}${bmParam}`,
  );
  if (!res.ok) {
    throw new Error(`GET /dashboard/pnl failed: ${res.status} ${res.statusText}`);
  }
  const body: DashboardPnlResponse = await res.json();
  return body.buckets;
}

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

/**
 * Indexer checkpoint-ingestion progress (SO-107). Proxied by api-service from
 * the indexer's `GET /progress`. Mirrors `api-service::state::IndexerProgress`.
 * Checkpoint sequence numbers are well within JS safe-integer range, so plain
 * `number` is fine here. `tip_checkpoint` is null until the indexer has polled
 * the chain tip at least once.
 */
export type IndexerProgress = {
  start_checkpoint: number;
  current_checkpoint: number;
  tip_checkpoint: number | null;
  rate_checkpoints_per_sec: number;
  caught_up: boolean;
};

export async function fetchIndexerProgress(): Promise<IndexerProgress> {
  const res = await fetch(`${API_BASE_URL}/indexer/progress`);
  if (!res.ok) {
    throw new Error(
      `GET /indexer/progress failed: ${res.status} ${res.statusText}`,
    );
  }
  return (await res.json()) as IndexerProgress;
}

/**
 * One row of the connected wallet's activity feed. Mirrors
 * `api-service::handlers::events::EventDto`. The feed is already specialised
 * to this wallet's perspective and sorted newest-first.
 *
 * The values are structured (symbol/strike/amount/signed value); the
 * frontend composes the human title/body in `state/activity.ts`.
 */
export type EventDto = {
  id: string;
  ts_ms: number;
  ts_iso: string;
  /** EVENT_TYPE_META key: position_opened | exercise | claim | deposit | withdraw. */
  type: string;
  /** writer | trader | account. */
  side: string;
  /** confirmed | pending | … (all live events are confirmed). */
  status: string;
  bucket_id: string | null;
  asset_symbol: string | null;
  settlement_symbol: string | null;
  strike: number | null;
  expiry_ms: number | null;
  /** Scaled underlying size (write/buy/exercise amount). */
  amount: number | null;
  /** Signed value for the UI's {delta, unit}. */
  value_delta: number | null;
  value_unit: string | null;
  /** Always null for now — the indexer doesn't carry the tx digest. */
  tx_hash: string | null;
};

export type EventsResponse = { events: EventDto[] };

export async function fetchActivity(wallet: string): Promise<EventDto[]> {
  const res = await fetch(
    `${API_BASE_URL}/events?wallet=${encodeURIComponent(wallet)}`,
  );
  if (!res.ok) {
    throw new Error(`GET /events failed: ${res.status} ${res.statusText}`);
  }
  const body: EventsResponse = await res.json();
  return body.events;
}
