//! Off-chain mirror of the Move events in §3.5.
//!
//! These are produced by the indexer (parsed out of Sui's event stream;
//! synthesized by the stub source until the contracts are deployed) and
//! consumed by anything downstream: the quoting service's state, frontends,
//! historical readers.
//!
//! The structs are JSON-shaped — they're not BCS-signed, just transported
//! over WS/JSON. Numeric fields use the decimal-string adapters to dodge JS
//! precision loss.

use serde::{Deserialize, Serialize};

use super::asset::AssetType;
use super::coding::{u128_string, u64_string};
use super::ids::{ObjectId, SuiAddress};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCreated {
    pub bucket_id: ObjectId,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
    /// Scaled strike. Real ratio = `strike / 10^strike_scale`
    /// (settlement-smallest-units per underlying-smallest-unit). u128 to
    /// match the on-chain Bucket field.
    #[serde(with = "u128_string")]
    pub strike: u128,
    /// 0..=9. Caps at 9 to match Pyth's normalized convention.
    pub strike_scale: u8,
}

impl BucketCreated {
    /// Real ratio as an f64. Lossy for very large strikes; convenient
    /// for log lines and UI. On-chain math always uses the raw integers.
    pub fn strike_as_f64(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteExecuted {
    pub bucket_id: ObjectId,
    pub signer_account_id: ObjectId,
    pub signer_token_recipient: SuiAddress,
    pub executor: SuiAddress,
    pub position_recipient: SuiAddress,
    pub call_token_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exercised {
    pub bucket_id: ObjectId,
    pub exerciser: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub settlement_paid: u64,
    #[serde(with = "u128_string")]
    pub cursor_after: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redeemed {
    pub bucket_id: ObjectId,
    pub position_id: ObjectId,
    pub redeemer: SuiAddress,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub underlying_returned: u64,
    #[serde(with = "u64_string")]
    pub settlement_returned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredOptionBurned {
    pub bucket_id: ObjectId,
    pub burner: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCleaned {
    pub bucket_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCreated {
    pub account_id: ObjectId,
    pub owner: SuiAddress,
    /// Tag for the registered signing key. BCS-encodes as a single u8;
    /// must match the on-chain struct field order in `events.move`.
    pub signing_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signing_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDeposit {
    pub account_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountWithdraw {
    pub account_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKeyRotated {
    pub account_id: ObjectId,
    pub new_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub new_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeUpdated {
    #[serde(with = "u64_string")]
    pub old_bps: u64,
    #[serde(with = "u64_string")]
    pub new_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreasuryWithdrawn {
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
    pub recipient: SuiAddress,
}

/// Tagged union over every event the indexer may publish.
///
/// The variant name is what shows up as `"type"` over the wire; the payload
/// rides under `"payload"`. This is the same envelope shape as the WS
/// retail/MM messages so a generic event reader can treat both alike.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ChainEvent {
    BucketCreated(BucketCreated),
    WriteExecuted(WriteExecuted),
    Exercised(Exercised),
    Redeemed(Redeemed),
    ExpiredOptionBurned(ExpiredOptionBurned),
    BucketCleaned(BucketCleaned),
    AccountCreated(AccountCreated),
    AccountDeposit(AccountDeposit),
    AccountWithdraw(AccountWithdraw),
    SigningKeyRotated(SigningKeyRotated),
    FeeUpdated(FeeUpdated),
    TreasuryWithdrawn(TreasuryWithdrawn),
}

/// An envelope wrapping a `ChainEvent` with the ordering metadata the indexer
/// emits. The `sequence` is monotonic across the stream so consumers can
/// detect gaps and resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedEvent {
    #[serde(with = "u64_string")]
    pub sequence: u64,
    #[serde(with = "u64_string")]
    pub timestamp_ms: u64,
    pub event: ChainEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_event_tagged_envelope() {
        let evt = ChainEvent::BucketCreated(BucketCreated {
            bucket_id: ObjectId::new([0x01; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            expiry_ms: 1_748_534_400_000,
            strike: 50_000_000_000,
            strike_scale: 0,
        });
        let v: serde_json::Value = serde_json::to_value(&evt).unwrap();
        assert_eq!(v["type"], "BucketCreated");
        assert_eq!(v["payload"]["asset_type"], "BTC");
        assert_eq!(v["payload"]["strike"], "50000000000");
        assert_eq!(v["payload"]["strike_scale"], 0);

        let back: ChainEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn strike_as_f64_applies_scale() {
        let ev = BucketCreated {
            bucket_id: ObjectId::new([0; 32]),
            asset_type: AssetType::new("DEEP"),
            settlement_type: AssetType::new("USDC"),
            expiry_ms: 0,
            strike: 15_000,
            strike_scale: 5,
        };
        // scale=5 → divisor 100_000 → 15_000 / 100_000 = 0.15
        assert!((ev.strike_as_f64() - 0.15).abs() < 1e-12);
    }

    #[test]
    fn indexed_envelope_round_trips() {
        let env = IndexedEvent {
            sequence: 42,
            timestamp_ms: 1,
            event: ChainEvent::FeeUpdated(FeeUpdated { old_bps: 0, new_bps: 50 }),
        };
        let j = serde_json::to_string(&env).unwrap();
        let back: IndexedEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, env);
    }
}
