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
//! - [`IndexerStream`] — indexer → consumers (one-way push of events plus the
//!   occasional snapshot).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::asset::AssetType;
use super::coding::{u128_string, u64_string};
use super::events::IndexedEvent;
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
    #[serde(with = "crate::protocol_types::coding::bytes_hex")]
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
    #[serde(with = "crate::protocol_types::coding::bytes_hex")]
    pub signing_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResponsePayload {
    #[serde(with = "crate::protocol_types::coding::bytes_hex")]
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmQuotePayload {
    pub quote: Quote,
    #[serde(with = "crate::protocol_types::coding::bytes_hex")]
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
    #[serde(with = "crate::protocol_types::coding::bytes_hex")]
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
    /// Bucket's on-chain strike — settlement raw-units per underlying
    /// raw-unit. MMs price against this so they don't have to track
    /// bucket state separately.
    #[serde(with = "u64_string")]
    pub strike: u64,
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
// indexer → consumers (quoting service, frontends)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IndexerStream {
    /// Initial backfill — the consumer's resume cursor.
    Snapshot {
        payload: IndexerSnapshotPayload,
    },
    /// Single live event.
    Event {
        payload: IndexedEvent,
    },
    /// Periodic stream marker; even if there are no events the consumer can
    /// detect partition.
    Heartbeat {
        #[serde(with = "u64_string")]
        latest_sequence: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerSnapshotPayload {
    #[serde(with = "u64_string")]
    pub latest_sequence: u64,
    pub events: Vec<IndexedEvent>,
}

/// Subscribe request the consumer sends to the indexer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IndexerSubscribe {
    /// Stream everything from `after_sequence` (exclusive). 0 means "from the
    /// beginning".
    Subscribe {
        #[serde(with = "u64_string")]
        after_sequence: u64,
    },
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
                expiry_ms: 1_900_000_000_000,
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"deadline_ms\":\"1748534400000\""));
        assert!(s.contains("\"strike\":\"500\""));
        assert!(s.contains("\"expiry_ms\":\"1900000000000\""));
        let back: ServiceToMm = serde_json::from_str(&s).unwrap();
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
