// Wire types for the quoting-service WebSocket protocol.
//
// Mirrors `rust-backend/crates/protocol-types/src/messages.rs`. Numeric
// fields the Rust side serializes as decimal strings (u64/u128) stay as
// strings here — parse to BigInt or Number at the consumer if needed.

export type RetailRole = "writer" | "trader" | "account";
export type Side = "writer" | "trader";

/** A bucket's full economic identity — the mirror of Move's
 *  `bucket_registry::BucketKey` and of `protocol_types::bucket_spec`.
 *
 *  `asset` and `settlement` are CHAIN-FORM type strings: address zero-padded
 *  to 64 hex chars with NO `0x` prefix, which is what a Move `TypeName`
 *  BCS-encodes to. That is not the form the rest of the frontend passes
 *  around, so build specs with `bucketSpecFor` rather than by hand — a
 *  `0x`-prefixed type here produces different signed bytes and the MM's
 *  signature stops verifying. */
export type BucketSpec = {
  asset: string;
  settlement: string;
  /** u64 decimal string, minute-aligned. */
  expiry_ms: string;
  /** u64 decimal string — normalized strike significand. */
  sig: string;
  /** Normalized strike exponent; real strike = sig / 10^exp. */
  exp: number;
  is_put: boolean;
};

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
  /** The bucket's economic identity — what the MM actually priced and what
   *  the chain verifies against the bucket's own fields. A quote binds this
   *  rather than an object id, because the bucket may not exist until the
   *  transaction that fills the quote creates it. */
  spec: BucketSpec;
  /** u128 decimal string. The fill is refused if the bucket already has more
   *  than this written ahead of it; `2^128-1` opts out. */
  max_total_written: string;
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
  spec: BucketSpec;
  /** Present only once the bucket exists on chain; `null` while the taker's
   *  own transaction is what will create it. Informational — the quotes bind
   *  the spec. */
  bucket_id: string | null;
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
  spec: BucketSpec;
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
      payload: { spec: BucketSpec; write_amount: string; side: Side };
    }
  | {
      type: "BulkViewRFQRequest";
      request_id: string;
      payload: { specs: BucketSpec[]; write_amount: string; side: Side };
    }
  | { type: "Pong" };
