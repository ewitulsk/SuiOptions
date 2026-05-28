//! Event-type strings + BCS dispatch.
//!
//! Each Move event we care about has a fully-qualified type string of the
//! form `{package_id}::events::{StructName}`. The Sui ingestion framework
//! hands us the type as a string; we match against this table to decide
//! whether (and how) to deserialize the event's BCS bytes.
//!
//! Move source: `contracts/sources/events.move`.
//!
//! The struct layouts on the protocol-types side were defined to BCS-match
//! the Move structs exactly (see `protocol_types::quote::tests::
//! bcs_layout_is_byte_exact` for the canonical example), so we just call
//! `bcs::from_bytes::<X>(...)` straight from event bytes — no field-by-field
//! conversion needed.

use anyhow::{Context, Result};

use protocol_types::events::{
    AccountCreated, AccountDeposit, AccountWithdraw, BucketCleaned, BucketCreated, ChainEvent,
    Exercised, ExpiredOptionBurned, FeeUpdated, Redeemed, SigningKeyRotated, TreasuryWithdrawn,
    WriteExecuted,
};

const EVENTS_MODULE: &str = "events";

/// All the event type strings the indexer subscribes to, derived from the
/// runtime `package_id`. Constructed once at boot.
#[derive(Debug, Clone)]
pub struct EventTypes {
    pub bucket_created: String,
    pub write_executed: String,
    pub exercised: String,
    pub redeemed: String,
    pub expired_option_burned: String,
    pub bucket_cleaned: String,
    pub account_created: String,
    pub account_deposit: String,
    pub account_withdraw: String,
    pub signing_key_rotated: String,
    pub fee_updated: String,
    pub treasury_withdrawn: String,
}

impl EventTypes {
    pub fn for_package(package_id: &str) -> Self {
        let mk = |name: &str| format!("{package_id}::{EVENTS_MODULE}::{name}");
        Self {
            bucket_created: mk("BucketCreated"),
            write_executed: mk("WriteExecuted"),
            exercised: mk("Exercised"),
            redeemed: mk("Redeemed"),
            expired_option_burned: mk("ExpiredOptionBurned"),
            bucket_cleaned: mk("BucketCleaned"),
            account_created: mk("AccountCreated"),
            account_deposit: mk("AccountDeposit"),
            account_withdraw: mk("AccountWithdraw"),
            signing_key_rotated: mk("SigningKeyRotated"),
            fee_updated: mk("FeeUpdated"),
            treasury_withdrawn: mk("TreasuryWithdrawn"),
        }
    }

    pub fn all_strings(&self) -> [&str; 12] {
        [
            &self.bucket_created,
            &self.write_executed,
            &self.exercised,
            &self.redeemed,
            &self.expired_option_burned,
            &self.bucket_cleaned,
            &self.account_created,
            &self.account_deposit,
            &self.account_withdraw,
            &self.signing_key_rotated,
            &self.fee_updated,
            &self.treasury_withdrawn,
        ]
    }
}

/// Try to deserialize `contents` as the event identified by `type_str`.
/// Returns `Ok(Some(_))` if the type matched and decoding succeeded,
/// `Ok(None)` if the type is not one of ours (caller drops the event),
/// `Err(_)` if the type matched but BCS decoding failed (caller logs and
/// continues — a malformed event of a known type is a chain/indexer bug).
pub fn dispatch(types: &EventTypes, type_str: &str, contents: &[u8]) -> Result<Option<ChainEvent>> {
    macro_rules! decode {
        ($variant:ident, $ty:ty) => {{
            let parsed: $ty = bcs::from_bytes(contents)
                .with_context(|| format!("bcs decode of {} ({} bytes)", stringify!($ty), contents.len()))?;
            Ok(Some(ChainEvent::$variant(parsed)))
        }};
    }

    if type_str == types.bucket_created {
        decode!(BucketCreated, BucketCreated)
    } else if type_str == types.write_executed {
        decode!(WriteExecuted, WriteExecuted)
    } else if type_str == types.exercised {
        decode!(Exercised, Exercised)
    } else if type_str == types.redeemed {
        decode!(Redeemed, Redeemed)
    } else if type_str == types.expired_option_burned {
        decode!(ExpiredOptionBurned, ExpiredOptionBurned)
    } else if type_str == types.bucket_cleaned {
        decode!(BucketCleaned, BucketCleaned)
    } else if type_str == types.account_created {
        decode!(AccountCreated, AccountCreated)
    } else if type_str == types.account_deposit {
        decode!(AccountDeposit, AccountDeposit)
    } else if type_str == types.account_withdraw {
        decode!(AccountWithdraw, AccountWithdraw)
    } else if type_str == types.signing_key_rotated {
        decode!(SigningKeyRotated, SigningKeyRotated)
    } else if type_str == types.fee_updated {
        decode!(FeeUpdated, FeeUpdated)
    } else if type_str == types.treasury_withdrawn {
        decode!(TreasuryWithdrawn, TreasuryWithdrawn)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::asset::AssetType;
    use protocol_types::ids::{ObjectId, SuiAddress};

    const PKG: &str = "0x9584b7c2890c52fc0f4c678cd96a219df8081dfa04d78428bf6c29213fb3f090";

    fn types() -> EventTypes {
        EventTypes::for_package(PKG)
    }

    #[test]
    fn type_strings_match_move_module_path() {
        let t = types();
        assert_eq!(t.bucket_created, format!("{PKG}::events::BucketCreated"));
        assert_eq!(t.write_executed, format!("{PKG}::events::WriteExecuted"));
        assert_eq!(t.account_deposit, format!("{PKG}::events::AccountDeposit"));
    }

    #[test]
    fn dispatch_decodes_bucket_created() {
        let t = types();
        let evt = BucketCreated {
            bucket_id: ObjectId::new([0x42; 32]),
            asset_type: AssetType::new("0x2::sui::SUI"),
            settlement_type: AssetType::new("0x123::usdc::USDC"),
            expiry_ms: 1_700_000_000_000,
            strike: 50_000_000_000,
            strike_scale: 2,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        let got = dispatch(&t, &t.bucket_created, &bytes).unwrap();
        match got {
            Some(ChainEvent::BucketCreated(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected BucketCreated, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_decodes_write_executed() {
        let t = types();
        let evt = WriteExecuted {
            bucket_id: ObjectId::new([0x11; 32]),
            signer_account_id: ObjectId::new([0x22; 32]),
            signer_token_recipient: SuiAddress::new([0x33; 32]),
            executor: SuiAddress::new([0x44; 32]),
            position_recipient: SuiAddress::new([0x55; 32]),
            call_token_recipient: SuiAddress::new([0x66; 32]),
            write_amount: 10_000,
            gross_premium: 500,
            fee: 5,
            net_premium: 495,
            range_start: 12_345,
            range_end: 22_345,
            nonce: 7,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        let got = dispatch(&t, &t.write_executed, &bytes).unwrap();
        match got {
            Some(ChainEvent::WriteExecuted(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected WriteExecuted, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_ignores_unknown_type() {
        let t = types();
        let got = dispatch(&t, "0xdead::other::Whatever", &[1, 2, 3]).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn dispatch_errors_on_malformed_bytes_of_known_type() {
        let t = types();
        // BucketCreated needs at least 32 + a vector-prefix + 32 + … bytes;
        // 3 random bytes should fail.
        let res = dispatch(&t, &t.bucket_created, &[1, 2, 3]);
        assert!(res.is_err());
    }
}
