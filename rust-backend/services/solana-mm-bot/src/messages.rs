//! MM-side WebSocket wire frames — a field-exact mirror of the
//! solana-quoting-service `messages` module (the counterparty on this
//! socket; see docs/solana/backend/05-solana-quoting-service.md).
//!
//! The quoting service is a main-workspace binary, so its message module
//! isn't importable from this standalone workspace — the subset the bot
//! speaks (MM ↔ service) is mirrored here, with the same envelope
//! (`{"type": ..., "request_id"?: ..., "payload": ...}`), the same serde
//! encodings ([`crate::coding`]) and `protocol_types::sides` reused so the
//! JSON stays byte-identical. Quote payloads use
//! [`solana_tx::quote::QuoteWire`] — the canonical wire form golden-tested
//! against the program crate.

use serde::{Deserialize, Serialize};

use protocol_types::sides::{MmRole, Side};
use solana_tx::quote::QuoteWire;

use crate::coding::{bytes_hex, u64_string};

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
    /// [`BulkViewRFQBroadcast`](ServiceToMm::BulkViewRFQBroadcast). No
    /// nonce is consumed and nothing is signed.
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
    /// The MM's MmAccount address (base58).
    pub account_id: String,
    /// On-chain scheme tag for `signing_pubkey` (0 = Ed25519, the only
    /// scheme in program v1).
    #[serde(default)]
    pub signing_scheme: u8,
    #[serde(with = "bytes_hex")]
    pub signing_pubkey: Vec<u8>,
    /// Opt-in to receiving unsigned bulk-view RFQs.
    #[serde(default)]
    pub bulk_view: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResponsePayload {
    #[serde(with = "bytes_hex")]
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmQuotePayload {
    pub quote: QuoteWire,
    #[serde(with = "bytes_hex")]
    pub signature: Vec<u8>,
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
    BulkViewRFQBroadcast {
        request_id: String,
        payload: BulkViewRfqBroadcastPayload,
    },
    AccountStateUpdate {
        payload: serde_json::Value,
    },
    ReservationConfirmed {
        request_id: String,
        payload: serde_json::Value,
    },
    ReservationReleased {
        request_id: String,
        payload: serde_json::Value,
    },
    Error {
        request_id: Option<String>,
        payload: ErrorPayload,
    },
    Ping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallengePayload {
    #[serde(with = "bytes_hex")]
    pub challenge: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthAckPayload {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

/// Service → MM: a quote is wanted on `bucket_id` for `write_amount` on
/// the given `side`. Carries **only the bucket address** — never its
/// strike, expiry, or mints. The bot resolves those itself from
/// solana-api-service (its own trust boundary).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqBroadcastPayload {
    pub bucket_id: String,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewRfqBroadcastPayload {
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    pub side: Side,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    pub bucket_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewQuotePayload {
    pub premiums: Vec<BulkViewMmPremium>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkViewMmPremium {
    pub bucket_id: String,
    #[serde(with = "u64_string")]
    pub premium: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_matches_quoting_service_wire_shape() {
        let msg = MmToService::Hello {
            payload: MmHelloPayload {
                roles: vec![MmRole::TraderMm, MmRole::WriterMm],
                account_id: "acc111".into(),
                signing_scheme: 0,
                signing_pubkey: vec![0xaa; 32],
                bulk_view: true,
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"Hello\""));
        assert!(s.contains("\"trader_mm\""));
        assert!(s.contains("\"signing_scheme\":0"));
        assert!(s.contains(&format!("\"0x{}\"", "aa".repeat(32))));
        let back: MmToService = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rfq_broadcast_decodes_decimal_strings_and_base58_ids() {
        let raw = r#"{"type":"RFQBroadcast","request_id":"req-1","payload":{
            "bucket_id":"9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
            "write_amount":"10000000","side":"trader","deadline_ms":"1760000000000"}}"#;
        let ServiceToMm::RFQBroadcast { request_id, payload } =
            serde_json::from_str(raw).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(request_id, "req-1");
        assert_eq!(payload.write_amount, 10_000_000);
        assert_eq!(payload.side, Side::Trader);
        assert_eq!(payload.deadline_ms, 1_760_000_000_000);
    }

    #[test]
    fn quote_payload_carries_hex_signature_and_wire_quote() {
        let payload = MmQuotePayload {
            quote: QuoteWire {
                protocol_id: "p".into(),
                signer_account: "s".into(),
                signer_token_recipient: "r".into(),
                bucket: "b".into(),
                write_amount: 1,
                premium: 2,
                valid_until_ms: 3,
                nonce: 4,
            },
            signature: vec![0xff; 64],
        };
        let v = serde_json::to_value(MmToService::Quote {
            request_id: "req".into(),
            payload,
        })
        .unwrap();
        assert_eq!(v["payload"]["quote"]["premium"], "2");
        assert!(v["payload"]["signature"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
    }
}
