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
  signer_account_id: string;
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
  | { type: "Pong" };
