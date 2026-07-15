// Wire types for the quoting-service WebSocket protocol.
//
// Mirrors `rust-backend/crates/protocol-types/src/messages.rs`. Numeric
// fields the Rust side serializes as decimal strings (u64/u128) stay as
// strings here — parse to BigInt or Number at the consumer if needed.

export type RetailRole = "writer" | "trader" | "account";
export type Side = "writer" | "trader";

export type Quote = {
  /** Hex-encoded protocol domain separator. */
  protocol_id: string;
  /** The MM's `QuoteSigner` object (was `signer_account_id`). */
  signer_id: string;
  /** Collateral routing — signed fields, passed through to `new_quote`
   *  verbatim. `release()` debits `collateral_source`; the call target is
   *  `{release_package}::{release_module}::release<T>`. */
  collateral_source: string;
  release_package: string;
  release_module: string;
  signer_token_recipient: string;
  bucket_id: string;
  /** u64 raw smallest-units of the underlying. */
  write_amount: string;
  /** u64 raw smallest-units of the settlement asset. */
  premium: string;
  /** u64 unix-ms. */
  valid_until_ms: string;
  /** u64 monotonic nonce per signer. */
  nonce: string;
};

export type RfqQuoteEntry = {
  quote: Quote;
  /** Hex-encoded signature. */
  signature: string;
  mm_id: string;
  mm_reputation: number;
};

export type RfqResponsePayload = {
  bucket_id: string;
  write_amount: string;
  /** Already sorted best-price-first for the retail side. */
  quotes: RfqQuoteEntry[];
};

export type ErrorPayload = {
  code: string;
  message: string;
};

// -- bulk-view (indicative tile premiums) --

/** One averaged indicative premium for a bucket. */
export type BulkViewPremium = {
  bucket_id: string;
  /** u64 raw settlement smallest-units — mean of responding MMs. */
  premium: string;
  /** How many MMs contributed to the average. */
  mm_count: number;
  /** Served from a past-TTL cache entry (a refresh is running in the background). */
  stale: boolean;
  /** u64 ms since the cached value was fetched. */
  cache_age_ms: string;
};

export type BulkViewResponsePayload = {
  write_amount: string;
  /** One entry per bucket that had a value; buckets no MM priced are omitted. */
  premiums: BulkViewPremium[];
};

// -- inbound (service → retail) --

export type ServiceToRetail =
  | { type: "HelloAck"; payload: { session_id: string } }
  | {
      type: "BucketUpdate";
      payload: {
        bucket_id: string;
        total_written: string;
        exercise_cursor: string;
        expiry_ms: string;
      };
    }
  | { type: "RFQResponse"; request_id: string; payload: RfqResponsePayload }
  | {
      type: "BulkViewRFQResponse";
      request_id: string;
      payload: BulkViewResponsePayload;
    }
  | { type: "Error"; request_id?: string | null; payload: ErrorPayload }
  | { type: "Ping" };

// -- outbound (retail → service) --

export type RetailToService =
  | { type: "Hello"; payload: { role: RetailRole; version: string } }
  | { type: "SubscribeBuckets"; payload: { bucket_ids: string[] } }
  | {
      type: "RFQRequest";
      request_id: string;
      payload: { bucket_id: string; write_amount: string; side: Side };
    }
  | {
      type: "BulkViewRFQRequest";
      request_id: string;
      payload: { bucket_ids: string[]; write_amount: string; side: Side };
    }
  | { type: "Pong" };
