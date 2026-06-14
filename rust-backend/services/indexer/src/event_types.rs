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
use serde::Deserialize;

use protocol_types::asset::AssetType;
use protocol_types::events::{
    AccountCreated, AccountDeposit, AccountWithdraw, BucketCleaned, BucketCreated,
    BucketInvalidated, BucketRevalidated, ChainEvent, CollateralizedWrite, Exercised,
    ExpiredOptionBurned, FeeUpdated, InstantWithdraw, Redeemed, RfqBid, RfqCreated,
    RfqExpiredUnsold, RfqSettled, SharesClaimed, SigningKeyRotated, SwapRfqBid, SwapRfqCreated,
    SwapRfqSettled, SwapRfqUnfilled, TreasuryWithdrawn, VaultBucketSelected, VaultConfigUpdated,
    VaultCreated, VaultDeposit, VaultDepositsPaused, VaultFeesCharged, VaultPositionRedeemed,
    VaultRoundFinalized, WithdrawCompleted, WithdrawInitiated, WriteExecuted,
};
use protocol_types::ids::{ObjectId, SuiAddress};

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
    pub bucket_invalidated: String,
    pub bucket_revalidated: String,
    pub account_created: String,
    pub account_deposit: String,
    pub account_withdraw: String,
    pub signing_key_rotated: String,
    pub fee_updated: String,
    pub treasury_withdrawn: String,
    // Write-core / RFQ events (guide docs 01–02).
    pub collateralized_write: String,
    pub rfq_created: String,
    pub rfq_bid: String,
    pub rfq_settled: String,
    pub rfq_expired_unsold: String,
    // Proceeds-swap auction events (swap_auction.move).
    pub swap_rfq_created: String,
    pub swap_rfq_bid: String,
    pub swap_rfq_settled: String,
    pub swap_rfq_unfilled: String,
    // Vault events (guide doc 03).
    pub vault_created: String,
    pub vault_deposit: String,
    pub shares_claimed: String,
    pub withdraw_initiated: String,
    pub withdraw_completed: String,
    pub instant_withdraw: String,
    pub vault_bucket_selected: String,
    pub vault_position_redeemed: String,
    pub vault_proceeds_swapped: String,
    pub vault_fees_charged: String,
    pub vault_round_finalized: String,
    pub vault_config_updated: String,
    pub vault_deposits_paused: String,
    /// Prefix of DeepBook's generic `pool::PoolCreated<Base, Quote>` event
    /// (SO-152). Built from DeepBook's ORIGINAL package id — Sui resolves
    /// event/struct types to the first publish, not the upgraded package
    /// that calls target. `None` on networks without a DeepBook deployment.
    pub deepbook_pool_created_prefix: Option<String>,
}

impl EventTypes {
    pub fn for_package(package_id: &str, deepbook_original_package_id: Option<&str>) -> Self {
        let mk = |name: &str| format!("{package_id}::{EVENTS_MODULE}::{name}");
        Self {
            bucket_created: mk("BucketCreated"),
            write_executed: mk("WriteExecuted"),
            exercised: mk("Exercised"),
            redeemed: mk("Redeemed"),
            expired_option_burned: mk("ExpiredOptionBurned"),
            bucket_cleaned: mk("BucketCleaned"),
            bucket_invalidated: mk("BucketInvalidated"),
            bucket_revalidated: mk("BucketRevalidated"),
            account_created: mk("AccountCreated"),
            account_deposit: mk("AccountDeposit"),
            account_withdraw: mk("AccountWithdraw"),
            signing_key_rotated: mk("SigningKeyRotated"),
            fee_updated: mk("FeeUpdated"),
            treasury_withdrawn: mk("TreasuryWithdrawn"),
            collateralized_write: mk("CollateralizedWrite"),
            rfq_created: mk("RfqCreated"),
            rfq_bid: mk("RfqBid"),
            rfq_settled: mk("RfqSettled"),
            rfq_expired_unsold: mk("RfqExpiredUnsold"),
            swap_rfq_created: mk("SwapRfqCreated"),
            swap_rfq_bid: mk("SwapRfqBid"),
            swap_rfq_settled: mk("SwapRfqSettled"),
            swap_rfq_unfilled: mk("SwapRfqUnfilled"),
            vault_created: mk("VaultCreated"),
            vault_deposit: mk("VaultDeposit"),
            shares_claimed: mk("SharesClaimed"),
            withdraw_initiated: mk("WithdrawInitiated"),
            withdraw_completed: mk("WithdrawCompleted"),
            instant_withdraw: mk("InstantWithdraw"),
            vault_bucket_selected: mk("VaultBucketSelected"),
            vault_position_redeemed: mk("VaultPositionRedeemed"),
            vault_proceeds_swapped: mk("VaultProceedsSwapped"),
            vault_fees_charged: mk("VaultFeesCharged"),
            vault_round_finalized: mk("VaultRoundFinalized"),
            vault_config_updated: mk("VaultConfigUpdated"),
            vault_deposits_paused: mk("VaultDepositsPaused"),
            deepbook_pool_created_prefix: deepbook_original_package_id
                .map(|pkg| format!("{pkg}::pool::PoolCreated<")),
        }
    }

    pub fn all_strings(&self) -> [&str; 35] {
        [
            &self.bucket_created,
            &self.write_executed,
            &self.exercised,
            &self.redeemed,
            &self.expired_option_burned,
            &self.bucket_cleaned,
            &self.bucket_invalidated,
            &self.bucket_revalidated,
            &self.account_created,
            &self.account_deposit,
            &self.account_withdraw,
            &self.signing_key_rotated,
            &self.fee_updated,
            &self.treasury_withdrawn,
            &self.collateralized_write,
            &self.rfq_created,
            &self.rfq_bid,
            &self.rfq_settled,
            &self.rfq_expired_unsold,
            &self.swap_rfq_created,
            &self.swap_rfq_bid,
            &self.swap_rfq_settled,
            &self.swap_rfq_unfilled,
            &self.vault_created,
            &self.vault_deposit,
            &self.shares_claimed,
            &self.withdraw_initiated,
            &self.withdraw_completed,
            &self.instant_withdraw,
            &self.vault_bucket_selected,
            &self.vault_position_redeemed,
            &self.vault_fees_charged,
            &self.vault_round_finalized,
            &self.vault_config_updated,
            &self.vault_deposits_paused,
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
    } else if type_str == types.bucket_invalidated {
        decode!(BucketInvalidated, BucketInvalidated)
    } else if type_str == types.bucket_revalidated {
        decode!(BucketRevalidated, BucketRevalidated)
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
    } else if type_str == types.collateralized_write {
        decode!(CollateralizedWrite, CollateralizedWrite)
    } else if type_str == types.rfq_created {
        decode!(RfqCreated, RfqCreated)
    } else if type_str == types.rfq_bid {
        decode!(RfqBid, RfqBid)
    } else if type_str == types.rfq_settled {
        decode!(RfqSettled, RfqSettled)
    } else if type_str == types.rfq_expired_unsold {
        decode!(RfqExpiredUnsold, RfqExpiredUnsold)
    } else if type_str == types.swap_rfq_created {
        decode!(SwapRfqCreated, SwapRfqCreated)
    } else if type_str == types.swap_rfq_bid {
        decode!(SwapRfqBid, SwapRfqBid)
    } else if type_str == types.swap_rfq_settled {
        decode!(SwapRfqSettled, SwapRfqSettled)
    } else if type_str == types.swap_rfq_unfilled {
        decode!(SwapRfqUnfilled, SwapRfqUnfilled)
    } else if type_str == types.vault_created {
        decode!(VaultCreated, VaultCreated)
    } else if type_str == types.vault_deposit {
        decode!(VaultDeposit, VaultDeposit)
    } else if type_str == types.shares_claimed {
        decode!(SharesClaimed, SharesClaimed)
    } else if type_str == types.withdraw_initiated {
        decode!(WithdrawInitiated, WithdrawInitiated)
    } else if type_str == types.withdraw_completed {
        decode!(WithdrawCompleted, WithdrawCompleted)
    } else if type_str == types.instant_withdraw {
        decode!(InstantWithdraw, InstantWithdraw)
    } else if type_str == types.vault_bucket_selected {
        decode!(VaultBucketSelected, VaultBucketSelected)
    } else if type_str == types.vault_position_redeemed {
        decode!(VaultPositionRedeemed, VaultPositionRedeemed)
    } else if type_str == types.vault_fees_charged {
        decode!(VaultFeesCharged, VaultFeesCharged)
    } else if type_str == types.vault_round_finalized {
        decode!(VaultRoundFinalized, VaultRoundFinalized)
    } else if type_str == types.vault_config_updated {
        decode!(VaultConfigUpdated, VaultConfigUpdated)
    } else if type_str == types.vault_deposits_paused {
        decode!(VaultDepositsPaused, VaultDepositsPaused)
    } else {
        Ok(None)
    }
}

/// DeepBook `PoolCreated` decoded but not yet tied to a bucket. The worker
/// resolves `base_asset_type` against known bucket call types and either
/// promotes this into `ChainEvent::DeepBookPoolCreated` or drops it (someone
/// else's pool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepBookPoolCreatedPartial {
    pub pool_id: ObjectId,
    pub base_asset_type: AssetType,
    pub quote_asset_type: AssetType,
    pub tick_size: u64,
    pub lot_size: u64,
    pub min_size: u64,
    pub taker_fee: u64,
    pub maker_fee: u64,
}

/// Raw BCS mirror of DeepBook's `pool::PoolCreated<Base, Quote>` payload.
/// Field order verified against the deployed testnet package (see
/// `tools/deepbook-pool-test/fixtures/pool_created.testnet.json` and
/// DEEPBOOK-FINDINGS.md §B). The type params are NOT in the payload — they
/// live in the event type string.
#[derive(Debug, Deserialize)]
struct RawDeepBookPoolCreated {
    pool_id: ObjectId,
    taker_fee: u64,
    maker_fee: u64,
    tick_size: u64,
    lot_size: u64,
    min_size: u64,
    #[allow(dead_code)]
    whitelisted_pool: bool,
    #[allow(dead_code)]
    treasury_address: SuiAddress,
}

/// Try to parse `type_str` + `contents` as a DeepBook `PoolCreated` event.
/// `Ok(None)` if the type doesn't match (or DeepBook isn't configured);
/// `Err` if it matches but the generics or BCS payload are malformed.
pub fn parse_deepbook_pool_created(
    types: &EventTypes,
    type_str: &str,
    contents: &[u8],
) -> Result<Option<DeepBookPoolCreatedPartial>> {
    let Some(prefix) = types.deepbook_pool_created_prefix.as_deref() else {
        return Ok(None);
    };
    if !type_str.starts_with(prefix) {
        return Ok(None);
    }
    let generics = type_str
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix('>'))
        .with_context(|| format!("malformed PoolCreated type string: {type_str}"))?;
    let params = split_top_level_generics(generics);
    let [base, quote] = params.as_slice() else {
        anyhow::bail!(
            "PoolCreated expects 2 type params, got {} in {type_str}",
            params.len()
        );
    };
    let raw: RawDeepBookPoolCreated = bcs::from_bytes(contents).with_context(|| {
        format!("bcs decode of DeepBook PoolCreated ({} bytes)", contents.len())
    })?;
    Ok(Some(DeepBookPoolCreatedPartial {
        pool_id: raw.pool_id,
        base_asset_type: AssetType::new(base.clone()),
        quote_asset_type: AssetType::new(quote.clone()),
        tick_size: raw.tick_size,
        lot_size: raw.lot_size,
        min_size: raw.min_size,
        taker_fee: raw.taker_fee,
        maker_fee: raw.maker_fee,
    }))
}

/// Split `A, B<C, D>, E` at top-level commas only (coin types are usually
/// concrete, but a depth counter keeps nested generics from breaking us).
fn split_top_level_generics(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in s.chars() {
        match c {
            '<' => {
                depth += 1;
                current.push(c);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::asset::AssetType;
    use protocol_types::ids::{ObjectId, SuiAddress};

    const PKG: &str = "0x9584b7c2890c52fc0f4c678cd96a219df8081dfa04d78428bf6c29213fb3f090";
    const DEEPBOOK_ORIG: &str =
        "0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982";

    fn types() -> EventTypes {
        EventTypes::for_package(PKG, Some(DEEPBOOK_ORIG))
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
            call_type: AssetType::new("0x9::call_0::CALL_0"),
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
            position_id: ObjectId::new([0xaa; 32]),
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
    fn dispatch_decodes_rfq_created() {
        let t = types();
        let evt = RfqCreated {
            rfq_id: ObjectId::new([0xaa; 32]),
            bucket_id: ObjectId::new([0xb1; 32]),
            origin: ObjectId::new([0xf0; 32]),
            amount: 250_000_000,
            reserve_premium: 47_619_000,
            deadline_ms: 1_700_000_900_000,
            max_deadline_ms: 1_700_001_500_000,
            min_increment_bps: 100,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.rfq_created, &bytes).unwrap() {
            Some(ChainEvent::RfqCreated(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected RfqCreated, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_vault_round_finalized() {
        let t = types();
        let evt = protocol_types::events::VaultRoundFinalized {
            vault_id: ObjectId::new([0xf1; 32]),
            round: 3,
            pps: 1_020_000_000_000,
            aum: 5_100_000_000,
            shares: 5_000_000_000,
            premium_collected: 80_000_000,
            premium_underlying: 23_000_000,
            withdrawals_owed: 102_000_000,
            shares_burned: 100_000_000,
            deposits_processed: 700_000_000,
            shares_minted: 686_274_509,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.vault_round_finalized, &bytes).unwrap() {
            Some(ChainEvent::VaultRoundFinalized(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected VaultRoundFinalized, got {other:?}"),
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

    /// Real-shaped PoolCreated payload bytes: BCS of a struct is the
    /// concatenation of its fields, so a tuple with the same field order
    /// produces identical bytes to the on-chain struct.
    fn pool_created_bytes(pool: [u8; 32]) -> Vec<u8> {
        bcs::to_bytes(&(
            ObjectId::new(pool),
            1_000_000u64, // taker_fee (0.1%)
            500_000u64,   // maker_fee (0.05%)
            10_000u64,    // tick_size
            1_000u64,     // lot_size
            10_000u64,    // min_size
            false,        // whitelisted_pool
            SuiAddress::new([0xb3; 32]), // treasury_address
        ))
        .unwrap()
    }

    #[test]
    fn parses_deepbook_pool_created_with_generics() {
        let t = types();
        let type_str = format!(
            "{DEEPBOOK_ORIG}::pool::PoolCreated<0x159c::call_3::CALL_3, 0x159c::tusdc::TUSDC>"
        );
        let got = parse_deepbook_pool_created(&t, &type_str, &pool_created_bytes([0xaa; 32]))
            .unwrap()
            .unwrap();
        assert_eq!(got.pool_id, ObjectId::new([0xaa; 32]));
        assert_eq!(got.base_asset_type.as_str(), "0x159c::call_3::CALL_3");
        assert_eq!(got.quote_asset_type.as_str(), "0x159c::tusdc::TUSDC");
        assert_eq!(got.tick_size, 10_000);
        assert_eq!(got.lot_size, 1_000);
        assert_eq!(got.min_size, 10_000);
        assert_eq!(got.taker_fee, 1_000_000);
        assert_eq!(got.maker_fee, 500_000);
    }

    #[test]
    fn pool_created_parse_ignores_foreign_and_unconfigured() {
        let t = types();
        // Wrong package → not ours.
        let foreign = "0xdead::pool::PoolCreated<0x1::a::A, 0x2::b::B>";
        assert!(parse_deepbook_pool_created(&t, foreign, &[1])
            .unwrap()
            .is_none());
        // Regular protocol event types don't match the prefix.
        assert!(parse_deepbook_pool_created(&t, &t.bucket_created, &[1])
            .unwrap()
            .is_none());
        // DeepBook unconfigured (devnet) → always None.
        let no_db = EventTypes::for_package(PKG, None);
        let real = format!("{DEEPBOOK_ORIG}::pool::PoolCreated<0x1::a::A, 0x2::b::B>");
        assert!(parse_deepbook_pool_created(&no_db, &real, &[1])
            .unwrap()
            .is_none());
    }

    #[test]
    fn pool_created_parse_errors_on_bad_payload_or_arity() {
        let t = types();
        let bad_arity = format!("{DEEPBOOK_ORIG}::pool::PoolCreated<0x1::a::A>");
        assert!(parse_deepbook_pool_created(&t, &bad_arity, &pool_created_bytes([0; 32])).is_err());
        let good_type = format!("{DEEPBOOK_ORIG}::pool::PoolCreated<0x1::a::A, 0x2::b::B>");
        assert!(parse_deepbook_pool_created(&t, &good_type, &[1, 2, 3]).is_err());
    }

    #[test]
    fn split_generics_handles_nesting() {
        assert_eq!(
            split_top_level_generics("0x1::a::A, 0x2::w::W<0x3::c::C, 0x4::d::D>"),
            vec![
                "0x1::a::A".to_string(),
                "0x2::w::W<0x3::c::C, 0x4::d::D>".to_string()
            ]
        );
    }
}
