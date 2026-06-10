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
    /// Org the bucket belongs to (creator's OrgCap.org_id).
    pub org_id: ObjectId,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    /// Fully-qualified type of the per-bucket fungible option coin
    /// (`Coin<call_type>`). BCS-matches the on-chain `TypeName` field.
    pub call_type: AssetType,
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

/// `flow` values mirroring `bucket.move`'s FLOW_WRITER / FLOW_TRADER.
pub const FLOW_WRITER: u8 = 0;
pub const FLOW_TRADER: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteExecuted {
    pub bucket_id: ObjectId,
    pub org_id: ObjectId,
    pub signer_account_id: ObjectId,
    /// Where the contract transferred the signer's minted asset (Coin<Call>
    /// in writer flow, Position in trader flow). The executor's side is
    /// returned to the PTB, so its final destination is PTB-decided and not
    /// recorded on-chain.
    pub signer_token_recipient: SuiAddress,
    pub executor: SuiAddress,
    pub position_id: ObjectId,
    /// 0 = writer flow, 1 = trader flow (see FLOW_WRITER / FLOW_TRADER).
    pub flow: u8,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    /// Org fee → Org balances; protocol fee → global Treasury.
    #[serde(with = "u64_string")]
    pub org_fee: u64,
    #[serde(with = "u64_string")]
    pub protocol_fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

impl WriteExecuted {
    /// Combined fee taken from the gross premium.
    pub fn total_fee(&self) -> u64 {
        self.org_fee + self.protocol_fee
    }

    /// The Position lands with the executor's PTB in writer flow and with
    /// the quote's recipient (the writer MM) in trader flow. Best-effort —
    /// in writer flow the PTB could route elsewhere, but conventionally the
    /// executor keeps it.
    pub fn position_recipient(&self) -> SuiAddress {
        if self.flow == FLOW_TRADER {
            self.signer_token_recipient
        } else {
            self.executor
        }
    }

    /// Mirror of `position_recipient` for the option coin side.
    pub fn call_token_recipient(&self) -> SuiAddress {
        if self.flow == FLOW_TRADER {
            self.executor
        } else {
            self.signer_token_recipient
        }
    }
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
pub struct BucketInvalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub actor: SuiAddress,
    /// true when gated by the protocol AdminCap override; false when by the
    /// bucket's OrgCap.
    pub by_admin: bool,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRevalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub actor: SuiAddress,
    pub by_admin: bool,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgCreated {
    pub org_id: ObjectId,
    pub name: String,
    #[serde(with = "u64_string")]
    pub fee_bps: u64,
    pub creator: SuiAddress,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgFeeUpdated {
    pub org_id: ObjectId,
    #[serde(with = "u64_string")]
    pub old_bps: u64,
    #[serde(with = "u64_string")]
    pub new_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgWithdraw {
    pub org_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolPauseSet {
    pub paused: bool,
    pub admin: SuiAddress,
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
pub struct ProtocolFeeUpdated {
    #[serde(with = "u64_string")]
    pub old_bps: u64,
    #[serde(with = "u64_string")]
    pub new_bps: u64,
}

/// No `recipient`: the withdrawn coin is returned to the PTB on-chain, so
/// the final destination is PTB-decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreasuryWithdrawn {
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

/// A DeepBook v3 pool created for one of OUR buckets' call coins (SO-152).
///
/// Unlike the other events this is NOT a BCS mirror of a single Move struct:
/// DeepBook's `pool::PoolCreated<Base, Quote>` carries the asset types only
/// in the event *type string* generics, and `bucket_id` is resolved by the
/// indexer (bucket whose `call_type` == the pool's base asset). The indexer
/// decodes the raw payload, parses the generics, and emits this enriched
/// form; pools whose base asset is not a known call coin are dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepBookPoolCreated {
    pub pool_id: ObjectId,
    /// Resolved off-chain: the bucket whose `call_type` is `base_asset_type`.
    pub bucket_id: ObjectId,
    pub base_asset_type: AssetType,
    pub quote_asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub tick_size: u64,
    #[serde(with = "u64_string")]
    pub lot_size: u64,
    #[serde(with = "u64_string")]
    pub min_size: u64,
    #[serde(with = "u64_string")]
    pub taker_fee: u64,
    #[serde(with = "u64_string")]
    pub maker_fee: u64,
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
    BucketInvalidated(BucketInvalidated),
    BucketRevalidated(BucketRevalidated),
    AccountCreated(AccountCreated),
    AccountDeposit(AccountDeposit),
    AccountWithdraw(AccountWithdraw),
    SigningKeyRotated(SigningKeyRotated),
    ProtocolFeeUpdated(ProtocolFeeUpdated),
    ProtocolPauseSet(ProtocolPauseSet),
    TreasuryWithdrawn(TreasuryWithdrawn),
    OrgCreated(OrgCreated),
    OrgFeeUpdated(OrgFeeUpdated),
    OrgWithdraw(OrgWithdraw),
    DeepBookPoolCreated(DeepBookPoolCreated),
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
            org_id: ObjectId::new([0x0a; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
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
            org_id: ObjectId::new([0; 32]),
            asset_type: AssetType::new("DEEP"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
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
            event: ChainEvent::ProtocolFeeUpdated(ProtocolFeeUpdated { old_bps: 0, new_bps: 50 }),
        };
        let j = serde_json::to_string(&env).unwrap();
        let back: IndexedEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn org_created_envelope_round_trips() {
        let env = IndexedEvent {
            sequence: 1,
            timestamp_ms: 2,
            event: ChainEvent::OrgCreated(OrgCreated {
                org_id: ObjectId::new([0x07; 32]),
                name: "acme".to_string(),
                fee_bps: 30,
                creator: SuiAddress::new([0x08; 32]),
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["event"]["type"], "OrgCreated");
        assert_eq!(v["event"]["payload"]["name"], "acme");
        assert_eq!(v["event"]["payload"]["fee_bps"], "30");
        let back: IndexedEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn write_executed_recipient_helpers_follow_flow() {
        let mut ev = WriteExecuted {
            bucket_id: ObjectId::new([0x11; 32]),
            org_id: ObjectId::new([0x12; 32]),
            signer_account_id: ObjectId::new([0x22; 32]),
            signer_token_recipient: SuiAddress::new([0x33; 32]),
            executor: SuiAddress::new([0x44; 32]),
            position_id: ObjectId::new([0xaa; 32]),
            flow: FLOW_WRITER,
            write_amount: 10_000,
            gross_premium: 500,
            org_fee: 3,
            protocol_fee: 5,
            net_premium: 492,
            range_start: 0,
            range_end: 10_000,
            nonce: 7,
        };
        assert_eq!(ev.total_fee(), 8);
        assert_eq!(ev.position_recipient(), ev.executor);
        assert_eq!(ev.call_token_recipient(), ev.signer_token_recipient);
        ev.flow = FLOW_TRADER;
        assert_eq!(ev.position_recipient(), ev.signer_token_recipient);
        assert_eq!(ev.call_token_recipient(), ev.executor);
    }
}
