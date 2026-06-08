//! WebSocket protocol from §5.4.
//!
//! Every wire frame is JSON of the form
//!
//! ```json
//! { "type": "<variant>", "request_id"?: "...", "payload": { ... } }
//! ```
//!
//! `request_id` is client-generated and correlates an `RFQRequest` with its
//! `RFQResponse`, an `RFQBroadcast` with the MM's `Quote`/`Decline`, etc.
//! Heartbeat (`Ping`/`Pong`) doesn't carry a request id.
//!
//! Direction:
//!
//! - [`RetailToService`] / [`ServiceToRetail`] — retail frontend ↔ quoting
//!   service.
//! - [`MmToService`] / [`ServiceToMm`] — market-maker bot ↔ quoting service.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::asset::AssetType;
use super::coding::{u128_string, u64_string};
use super::ids::ObjectId;
use super::quote::{Quote, SignedQuote};
use super::sides::{MmRole, RetailRole, Side};

// ---------------------------------------------------------------------------
// retail ↔ service
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RetailToService {
    Hello {
        payload: RetailHelloPayload,
    },
    SubscribeBuckets {
        payload: SubscribeBucketsPayload,
    },
    RFQRequest {
        request_id: String,
        payload: RfqRequestPayload,
    },
    /// Unsigned, non-executable request for indicative premiums across many
    /// buckets at once. Powers the tile display without spamming MMs with
    /// signable RFQs. See [`BulkViewRfqRequestPayload`].
    BulkViewRFQRequest {
        request_id: String,
        payload: BulkViewRfqRequestPayload,
    },
    Pong,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetailHelloPayload {
    pub role: RetailRole,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeBucketsPayload {
    pub bucket_ids: Vec<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqRequestPayload {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServiceToRetail {
    HelloAck {
        payload: HelloAckPayload,
    },
    BucketUpdate {
        payload: BucketUpdatePayload,
    },
    RFQResponse {
        request_id: String,
        payload: RfqResponsePayload,
    },
    /// Averaged indicative premiums for a [`BulkViewRFQRequest`](RetailToService::BulkViewRFQRequest).
    BulkViewRFQResponse {
        request_id: String,
        payload: BulkViewRfqResponsePayload,
    },
    Error {
        request_id: Option<String>,
        payload: ErrorPayload,
    },
    Ping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAckPayload {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketUpdatePayload {
    pub bucket_id: ObjectId,
    #[serde(with = "u128_string")]
    pub total_written: u128,
    #[serde(with = "u128_string")]
    pub exercise_cursor: u128,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RfqResponsePayload {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    /// Already sorted best-price-first for the retail user (highest premium
    /// for writer-side, lowest premium for trader-side).
    pub quotes: Vec<RfqQuoteEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RfqQuoteEntry {
    pub quote: Quote,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signature: Vec<u8>,
    pub mm_id: ObjectId,
    pub mm_reputation: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// MM ↔ service
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MmToService {
    Hello {
        payload: MmHelloPayload,
    },
    AuthResponse {
        payload: AuthResponsePayload,
    },
    Quote {
        request_id: String,
        payload: MmQuotePayload,
    },
    /// Unsigned indicative premiums in response to a
    /// [`BulkViewRFQBroadcast`](ServiceToMm::BulkViewRFQBroadcast). No nonce
    /// is consumed and nothing is signed — these never reach the chain.
    BulkViewQuote {
        request_id: String,
        payload: BulkViewQuotePayload,
    },
    Decline {
        request_id: String,
        payload: DeclinePayload,
    },
    Pong,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmHelloPayload {
    pub roles: Vec<MmRole>,
    pub account_id: ObjectId,
    /// Tag for the signing scheme used by `signing_pubkey` (and every
    /// signature this MM ships during the session). Must match the value
    /// registered on the Account on chain.
    pub signing_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signing_pubkey: Vec<u8>,
    /// Opt-in to receiving unsigned [`BulkViewRFQBroadcast`](ServiceToMm::BulkViewRFQBroadcast)
    /// requests. Defaults to false so an MM that doesn't set it is simply
    /// never sent bulk-view RFQs.
    #[serde(default)]
    pub bulk_view: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResponsePayload {
    #[serde(with = "crate::coding::bytes_hex")]
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmQuotePayload {
    pub quote: Quote,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signature: Vec<u8>,
}

impl MmQuotePayload {
    pub fn into_signed(self) -> SignedQuote {
        SignedQuote {
            quote: self.quote,
            signature: self.signature,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclinePayload {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServiceToMm {
    AuthChallenge {
        payload: AuthChallengePayload,
    },
    AuthAck {
        payload: AuthAckPayload,
    },
    RFQBroadcast {
        request_id: String,
        payload: RfqBroadcastPayload,
    },
    /// Price these buckets for tile display. Unsigned — the MM responds with
    /// [`BulkViewQuote`](MmToService::BulkViewQuote). Sent only to MMs that
    /// advertised `bulk_view = true`.
    BulkViewRFQBroadcast {
        request_id: String,
        payload: BulkViewRfqBroadcastPayload,
    },
    AccountStateUpdate {
        payload: AccountStateUpdatePayload,
    },
    ReservationConfirmed {
        request_id: String,
        payload: ReservationPayload,
    },
    ReservationReleased {
        request_id: String,
        payload: ReservationPayload,
    },
    Error {
        request_id: Option<String>,
        payload: ErrorPayload,
    },
    Ping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallengePayload {
    #[serde(with = "crate::coding::bytes_hex")]
    pub challenge: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthAckPayload {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqBroadcastPayload {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    /// Bucket's on-chain strike. Real ratio (settlement raw-units per
    /// underlying raw-unit) is `strike / 10^strike_scale`. MMs must
    /// normalize before plugging into a pricing model.
    #[serde(with = "u128_string")]
    pub strike: u128,
    /// 0..=9. See `BucketCreated::strike_scale`.
    pub strike_scale: u8,
    /// Bucket expiry as a Sui clock millisecond timestamp. The MM derives
    /// time-to-expiry from this directly instead of guessing.
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStateUpdatePayload {
    pub account_id: ObjectId,
    /// `asset_type → balance` (raw smallest-units, decimal-string in JSON).
    pub balances: BTreeMap<AssetType, U64Str>,
    pub active_reservations: BTreeMap<AssetType, U64Str>,
    pub available: BTreeMap<AssetType, U64Str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationPayload {
    pub account_id: ObjectId,
    #[serde(with = "u64_string")]
    pub nonce: u64,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

/// `u64` map-value newtype — the workspace coding adapter can't be applied
/// inside a `BTreeMap`'s value position directly (serde's `#[serde(with)]`
/// only lives on fields). Wrap in a transparent newtype that uses the same
/// adapter internally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct U64Str(pub u64);

impl From<u64> for U64Str {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<U64Str> for u64 {
    fn from(v: U64Str) -> Self {
        v.0
    }
}

impl Serialize for U64Str {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.serialize_str(&self.0.to_string())
        } else {
            ser.serialize_u64(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for U64Str {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            s.parse::<u64>().map(U64Str).map_err(serde::de::Error::custom)
        } else {
            u64::deserialize(de).map(U64Str)
        }
    }
}

// ---------------------------------------------------------------------------
// bulk-view RFQ (unsigned indicative premiums for tile display)
// ---------------------------------------------------------------------------

/// Retail → service: request indicative premiums for many buckets at one
/// write amount. Unlike `RFQRequest`, the result is unsigned and not
/// executable — it only drives the tile display, so it never reserves MM
/// balance or consumes a nonce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewRfqRequestPayload {
    pub bucket_ids: Vec<ObjectId>,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
}

/// Service → retail: averaged indicative premiums, one entry per bucket the
/// service had (or could fetch) a value for. Buckets no MM priced are omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewRfqResponsePayload {
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub premiums: Vec<BulkViewPremium>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewPremium {
    pub bucket_id: ObjectId,
    /// Mean of the responding MMs' premiums, settlement smallest-units.
    #[serde(with = "u64_string")]
    pub premium: u64,
    /// How many MMs contributed to the average.
    pub mm_count: u32,
    /// True if this value came from a cache entry past its TTL (a refresh was
    /// kicked off in the background; the next request carries the fresh value).
    pub stale: bool,
    /// Age of the cached value in ms (≈0 for a value just fetched).
    #[serde(with = "u64_string")]
    pub cache_age_ms: u64,
}

/// Service → MM: price these buckets at `write_amount`, no signing. Sent only
/// to MMs that advertised `bulk_view = true` in their Hello.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewRfqBroadcastPayload {
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    pub buckets: Vec<BulkViewBucket>,
}

/// One bucket in a [`BulkViewRfqBroadcastPayload`]. Carries the same pricing
/// inputs `RfqBroadcastPayload` does, minus the per-request `write_amount`
/// (shared across the batch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewBucket {
    pub bucket_id: ObjectId,
    #[serde(with = "u128_string")]
    pub strike: u128,
    pub strike_scale: u8,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
}

/// MM → service: indicative premiums for the requested buckets. Unsigned; no
/// nonce consumed. Buckets the MM declines to price are omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewQuotePayload {
    pub premiums: Vec<BulkViewMmPremium>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewMmPremium {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub premium: u64,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Recipient of a `ServiceToMm` after the service routes an RFQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationOutcome {
    Confirmed,
    Released,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfq_request_round_trips() {
        let msg = RetailToService::RFQRequest {
            request_id: "req-abc".into(),
            payload: RfqRequestPayload {
                bucket_id: ObjectId::new([0x01; 32]),
                write_amount: 10_000_000,
                side: Side::Writer,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "RFQRequest");
        assert_eq!(v["request_id"], "req-abc");
        assert_eq!(v["payload"]["side"], "writer");
        assert_eq!(v["payload"]["write_amount"], "10000000");

        let back: RetailToService = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn mm_hello_round_trips() {
        let msg = MmToService::Hello {
            payload: MmHelloPayload {
                roles: vec![MmRole::TraderMm, MmRole::WriterMm],
                account_id: ObjectId::new([0x02; 32]),
                signing_scheme: crate::SigningScheme::Ed25519,
                signing_pubkey: vec![0xaa; 32],
                bulk_view: true,
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"Hello\""));
        assert!(s.contains("\"trader_mm\""));
        assert!(s.contains("\"writer_mm\""));
        assert!(s.contains("\"signing_scheme\":\"ed25519\""));
        let back: MmToService = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn account_state_balances_use_decimal_strings() {
        let mut bal = BTreeMap::new();
        bal.insert(AssetType::new("USDC"), U64Str(1_000_000_000));
        bal.insert(AssetType::new("BTC"), U64Str(50_000_000));
        let msg = ServiceToMm::AccountStateUpdate {
            payload: AccountStateUpdatePayload {
                account_id: ObjectId::new([0x07; 32]),
                balances: bal.clone(),
                active_reservations: BTreeMap::new(),
                available: bal,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["payload"]["balances"]["USDC"], "1000000000");
        assert_eq!(v["payload"]["balances"]["BTC"], "50000000");

        let back: ServiceToMm = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rfq_broadcast_includes_deadline() {
        let msg = ServiceToMm::RFQBroadcast {
            request_id: "req-1".into(),
            payload: RfqBroadcastPayload {
                bucket_id: ObjectId::new([0x0a; 32]),
                write_amount: 5,
                side: Side::Writer,
                deadline_ms: 1_748_534_400_000,
                strike: 500,
                strike_scale: 0,
                expiry_ms: 1_900_000_000_000,
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"deadline_ms\":\"1748534400000\""));
        assert!(s.contains("\"strike\":\"500\""));
        assert!(s.contains("\"strike_scale\":0"));
        assert!(s.contains("\"expiry_ms\":\"1900000000000\""));
        let back: ServiceToMm = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn mm_hello_bulk_view_defaults_false_when_absent() {
        // Older MMs that predate the flag omit it entirely; it must parse.
        let json = r#"{"type":"Hello","payload":{"roles":["trader_mm"],"account_id":"0x0202020202020202020202020202020202020202020202020202020202020202","signing_scheme":"ed25519","signing_pubkey":"0xaa"}}"#;
        let back: MmToService = serde_json::from_str(json).unwrap();
        match back {
            MmToService::Hello { payload } => assert!(!payload.bulk_view),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn bulk_view_request_round_trips() {
        let msg = RetailToService::BulkViewRFQRequest {
            request_id: "bv-1".into(),
            payload: BulkViewRfqRequestPayload {
                bucket_ids: vec![ObjectId::new([0x01; 32]), ObjectId::new([0x02; 32])],
                write_amount: 5_000_000,
                side: Side::Writer,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "BulkViewRFQRequest");
        assert_eq!(v["payload"]["write_amount"], "5000000");
        assert_eq!(v["payload"]["side"], "writer");
        let back: RetailToService = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn bulk_view_broadcast_and_quote_round_trip() {
        let bc = ServiceToMm::BulkViewRFQBroadcast {
            request_id: "bv-1".into(),
            payload: BulkViewRfqBroadcastPayload {
                write_amount: 100,
                side: Side::Writer,
                deadline_ms: 1_748_534_400_000,
                buckets: vec![BulkViewBucket {
                    bucket_id: ObjectId::new([0x0a; 32]),
                    strike: 500,
                    strike_scale: 2,
                    expiry_ms: 1_900_000_000_000,
                }],
            },
        };
        let s = serde_json::to_string(&bc).unwrap();
        assert!(s.contains("\"type\":\"BulkViewRFQBroadcast\""));
        assert!(s.contains("\"write_amount\":\"100\""));
        assert!(s.contains("\"strike\":\"500\""));
        assert_eq!(serde_json::from_str::<ServiceToMm>(&s).unwrap(), bc);

        let q = MmToService::BulkViewQuote {
            request_id: "bv-1".into(),
            payload: BulkViewQuotePayload {
                premiums: vec![BulkViewMmPremium {
                    bucket_id: ObjectId::new([0x0a; 32]),
                    premium: 4242,
                }],
            },
        };
        let s = serde_json::to_string(&q).unwrap();
        assert!(s.contains("\"type\":\"BulkViewQuote\""));
        assert!(s.contains("\"premium\":\"4242\""));
        assert_eq!(serde_json::from_str::<MmToService>(&s).unwrap(), q);
    }

    #[test]
    fn bulk_view_response_round_trips() {
        let msg = ServiceToRetail::BulkViewRFQResponse {
            request_id: "bv-1".into(),
            payload: BulkViewRfqResponsePayload {
                write_amount: 100,
                premiums: vec![BulkViewPremium {
                    bucket_id: ObjectId::new([0x0a; 32]),
                    premium: 4242,
                    mm_count: 3,
                    stale: true,
                    cache_age_ms: 12_000,
                }],
            },
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "BulkViewRFQResponse");
        assert_eq!(v["payload"]["premiums"][0]["premium"], "4242");
        assert_eq!(v["payload"]["premiums"][0]["mm_count"], 3);
        assert_eq!(v["payload"]["premiums"][0]["stale"], true);
        assert_eq!(v["payload"]["premiums"][0]["cache_age_ms"], "12000");
        let back: ServiceToRetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn ping_serialises_as_bare_type() {
        let s = serde_json::to_string(&ServiceToMm::Ping).unwrap();
        assert_eq!(s, "{\"type\":\"Ping\"}");
        let back: ServiceToMm = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ServiceToMm::Ping);
    }
}
