//! Event-type strings + BCS dispatch.
//!
//! Each Move event we care about has a fully-qualified type string of the
//! form `{package_id}::events::{StructName}`, where the package id is one
//! of the FOUR packages of the contracts tree: `options_core` (buckets /
//! accounts / treasury), `auction` (the generic venue), `options_rfq` (the
//! option-RFQ adapter) and `options_vault`. The Sui ingestion framework
//! hands us the type as a string; we match against this table to decide
//! whether (and how) to deserialize the event's BCS bytes.
//!
//! Move source: `contracts/{core,auction,rfq,vault}/sources/events.move`.
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
    AccountCreated, AccountDeposit, AccountWithdraw, AuctionBid, AuctionCreated, AuctionSettled,
    AuctionUnfilled, BucketCleaned, BucketCreated, BucketInvalidated, BucketRevalidated,
    ChainEvent, CollateralizedWrite, Exercised, ExpiredOptionBurned, FeeUpdated, InstantWithdraw,
    PutBucketCleaned, PutBucketCreated, PutBucketInvalidated, PutBucketRevalidated,
    PutCollateralizedWrite, PutExercised, PutExpiredOptionBurned, PutRedeemed, PutRfqCreated,
    PutRfqExpiredUnsold, PutRfqSettled, PutWriteExecuted, Redeemed, RfqCreated, RfqExpiredUnsold,
    RfqSettled, SharesClaimed, SigningKeyRotated, SwapRfqSettled, SwapRfqUnfilled,
    TreasuryWithdrawn, VaultBucketSelected, VaultConfigUpdated, VaultCreated, VaultDeposit,
    VaultDepositsPaused, VaultFeesCharged, VaultPositionRedeemed, VaultRfqSettled, VaultRfqUnsold,
    VaultRoundFinalized, WithdrawCompleted, WithdrawInitiated, WriteExecuted,
};
use protocol_types::ids::{ObjectId, SuiAddress};

const EVENTS_MODULE: &str = "events";

/// The four published package ids the protocol's events resolve to.
/// All required — `main.rs` fails at boot when token-info is missing one.
#[derive(Debug, Clone, Copy)]
pub struct PackageIds<'a> {
    /// options_core.
    pub core: &'a str,
    /// Generic auction venue.
    pub auction: &'a str,
    /// options_rfq adapter.
    pub rfq: &'a str,
    /// options_vault.
    pub vault: &'a str,
}

/// All the event type strings the indexer subscribes to, derived from the
/// runtime package ids. Constructed once at boot.
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
    // Write-core events (guide docs 01–02).
    pub collateralized_write: String,
    // Generic auction venue events (auction package).
    pub auction_created: String,
    pub auction_bid: String,
    pub auction_settled: String,
    pub auction_unfilled: String,
    // Option-RFQ adapter events (options_rfq package).
    pub rfq_created: String,
    pub rfq_settled: String,
    pub rfq_expired_unsold: String,
    // Vault-coupled RFQ settles + proceeds-swap settles (options_vault).
    pub vault_rfq_settled: String,
    pub vault_rfq_unsold: String,
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
    pub vault_fees_charged: String,
    pub vault_round_finalized: String,
    pub vault_config_updated: String,
    /// Active-config snapshot at each finalize. Absent from the old
    /// single-package table (the store already handled the variant).
    pub vault_config_applied: String,
    pub vault_deposits_paused: String,
    // Cash-secured put events (mirror of the call/RFQ events above).
    pub put_bucket_created: String,
    pub put_write_executed: String,
    pub put_collateralized_write: String,
    pub put_exercised: String,
    pub put_redeemed: String,
    pub put_expired_option_burned: String,
    pub put_bucket_cleaned: String,
    pub put_bucket_invalidated: String,
    pub put_bucket_revalidated: String,
    pub put_rfq_created: String,
    pub put_rfq_settled: String,
    pub put_rfq_expired_unsold: String,
    /// Prefix of DeepBook's generic `pool::PoolCreated<Base, Quote>` event
    /// (SO-152). Built from DeepBook's ORIGINAL package id — Sui resolves
    /// event/struct types to the first publish, not the upgraded package
    /// that calls target. `None` on networks without a DeepBook deployment.
    pub deepbook_pool_created_prefix: Option<String>,
    /// DeepBook's non-generic `order_info::OrderFilled` type string (SO-209).
    /// Exact-matchable (zero type params); `None` without a DeepBook deploy.
    pub deepbook_order_filled: Option<String>,
}

impl EventTypes {
    pub fn for_packages(pkgs: PackageIds<'_>, deepbook_original_package_id: Option<&str>) -> Self {
        let core = |name: &str| format!("{}::{EVENTS_MODULE}::{name}", pkgs.core);
        let auction = |name: &str| format!("{}::{EVENTS_MODULE}::{name}", pkgs.auction);
        let rfq = |name: &str| format!("{}::{EVENTS_MODULE}::{name}", pkgs.rfq);
        let vault = |name: &str| format!("{}::{EVENTS_MODULE}::{name}", pkgs.vault);
        Self {
            bucket_created: core("BucketCreated"),
            write_executed: core("WriteExecuted"),
            exercised: core("Exercised"),
            redeemed: core("Redeemed"),
            expired_option_burned: core("ExpiredOptionBurned"),
            bucket_cleaned: core("BucketCleaned"),
            bucket_invalidated: core("BucketInvalidated"),
            bucket_revalidated: core("BucketRevalidated"),
            account_created: core("AccountCreated"),
            account_deposit: core("AccountDeposit"),
            account_withdraw: core("AccountWithdraw"),
            signing_key_rotated: core("SigningKeyRotated"),
            fee_updated: core("FeeUpdated"),
            treasury_withdrawn: core("TreasuryWithdrawn"),
            collateralized_write: core("CollateralizedWrite"),
            auction_created: auction("AuctionCreated"),
            auction_bid: auction("AuctionBid"),
            auction_settled: auction("AuctionSettled"),
            auction_unfilled: auction("AuctionUnfilled"),
            rfq_created: rfq("RfqCreated"),
            rfq_settled: rfq("RfqSettled"),
            rfq_expired_unsold: rfq("RfqExpiredUnsold"),
            vault_rfq_settled: vault("VaultRfqSettled"),
            vault_rfq_unsold: vault("VaultRfqUnsold"),
            swap_rfq_settled: vault("SwapRfqSettled"),
            swap_rfq_unfilled: vault("SwapRfqUnfilled"),
            vault_created: vault("VaultCreated"),
            vault_deposit: vault("VaultDeposit"),
            shares_claimed: vault("SharesClaimed"),
            withdraw_initiated: vault("WithdrawInitiated"),
            withdraw_completed: vault("WithdrawCompleted"),
            instant_withdraw: vault("InstantWithdraw"),
            vault_bucket_selected: vault("VaultBucketSelected"),
            vault_position_redeemed: vault("VaultPositionRedeemed"),
            vault_fees_charged: vault("VaultFeesCharged"),
            vault_round_finalized: vault("VaultRoundFinalized"),
            vault_config_updated: vault("VaultConfigUpdated"),
            vault_config_applied: vault("VaultConfigApplied"),
            vault_deposits_paused: vault("VaultDepositsPaused"),
            put_bucket_created: core("PutBucketCreated"),
            put_write_executed: core("PutWriteExecuted"),
            put_collateralized_write: core("PutCollateralizedWrite"),
            put_exercised: core("PutExercised"),
            put_redeemed: core("PutRedeemed"),
            put_expired_option_burned: core("PutExpiredOptionBurned"),
            put_bucket_cleaned: core("PutBucketCleaned"),
            put_bucket_invalidated: core("PutBucketInvalidated"),
            put_bucket_revalidated: core("PutBucketRevalidated"),
            put_rfq_created: rfq("PutRfqCreated"),
            put_rfq_settled: rfq("PutRfqSettled"),
            put_rfq_expired_unsold: rfq("PutRfqExpiredUnsold"),
            deepbook_pool_created_prefix: deepbook_original_package_id
                .map(|pkg| format!("{pkg}::pool::PoolCreated<")),
            deepbook_order_filled: deepbook_original_package_id
                .map(|pkg| format!("{pkg}::order_info::OrderFilled")),
        }
    }

    pub fn all_strings(&self) -> [&str; 51] {
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
            &self.auction_created,
            &self.auction_bid,
            &self.auction_settled,
            &self.auction_unfilled,
            &self.rfq_created,
            &self.rfq_settled,
            &self.rfq_expired_unsold,
            &self.vault_rfq_settled,
            &self.vault_rfq_unsold,
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
            &self.vault_config_applied,
            &self.vault_deposits_paused,
            &self.put_bucket_created,
            &self.put_write_executed,
            &self.put_collateralized_write,
            &self.put_exercised,
            &self.put_redeemed,
            &self.put_expired_option_burned,
            &self.put_bucket_cleaned,
            &self.put_bucket_invalidated,
            &self.put_bucket_revalidated,
            &self.put_rfq_created,
            &self.put_rfq_settled,
            &self.put_rfq_expired_unsold,
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
    } else if type_str == types.auction_created {
        decode!(AuctionCreated, AuctionCreated)
    } else if type_str == types.auction_bid {
        decode!(AuctionBid, AuctionBid)
    } else if type_str == types.auction_settled {
        decode!(AuctionSettled, AuctionSettled)
    } else if type_str == types.auction_unfilled {
        decode!(AuctionUnfilled, AuctionUnfilled)
    } else if type_str == types.rfq_created {
        decode!(RfqCreated, RfqCreated)
    } else if type_str == types.rfq_settled {
        decode!(RfqSettled, RfqSettled)
    } else if type_str == types.rfq_expired_unsold {
        decode!(RfqExpiredUnsold, RfqExpiredUnsold)
    } else if type_str == types.vault_rfq_settled {
        decode!(VaultRfqSettled, VaultRfqSettled)
    } else if type_str == types.vault_rfq_unsold {
        decode!(VaultRfqUnsold, VaultRfqUnsold)
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
    } else if type_str == types.vault_config_applied {
        decode!(VaultConfigApplied, protocol_types::events::VaultConfigApplied)
    } else if type_str == types.vault_deposits_paused {
        decode!(VaultDepositsPaused, VaultDepositsPaused)
    } else if type_str == types.put_bucket_created {
        decode!(PutBucketCreated, PutBucketCreated)
    } else if type_str == types.put_write_executed {
        decode!(PutWriteExecuted, PutWriteExecuted)
    } else if type_str == types.put_collateralized_write {
        decode!(PutCollateralizedWrite, PutCollateralizedWrite)
    } else if type_str == types.put_exercised {
        decode!(PutExercised, PutExercised)
    } else if type_str == types.put_redeemed {
        decode!(PutRedeemed, PutRedeemed)
    } else if type_str == types.put_expired_option_burned {
        decode!(PutExpiredOptionBurned, PutExpiredOptionBurned)
    } else if type_str == types.put_bucket_cleaned {
        decode!(PutBucketCleaned, PutBucketCleaned)
    } else if type_str == types.put_bucket_invalidated {
        decode!(PutBucketInvalidated, PutBucketInvalidated)
    } else if type_str == types.put_bucket_revalidated {
        decode!(PutBucketRevalidated, PutBucketRevalidated)
    } else if type_str == types.put_rfq_created {
        decode!(PutRfqCreated, PutRfqCreated)
    } else if type_str == types.put_rfq_settled {
        decode!(PutRfqSettled, PutRfqSettled)
    } else if type_str == types.put_rfq_expired_unsold {
        decode!(PutRfqExpiredUnsold, PutRfqExpiredUnsold)
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

/// DeepBook `OrderFilled` decoded but not yet tied to a bucket (SO-209). The
/// worker resolves `pool_id` against known bucket pools and either promotes it
/// into `ChainEvent::DeepBookOrderFilled` or drops it (a fill on a foreign pool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepBookOrderFilledPartial {
    pub pool_id: ObjectId,
    pub taker_balance_manager_id: ObjectId,
    pub maker_balance_manager_id: ObjectId,
    pub taker_is_bid: bool,
    pub base_quantity: u64,
    pub quote_quantity: u64,
    pub price: u64,
    pub taker_fee: u64,
    pub taker_fee_is_deep: bool,
    pub maker_fee: u64,
    pub maker_fee_is_deep: bool,
    pub timestamp_ms: u64,
}

/// Raw BCS mirror of DeepBook's `order_info::OrderFilled` payload. Field order
/// verified against the deployed testnet package (DEEPBOOK-FINDINGS.md §B and
/// `tools/deepbook-pool-test/fixtures/order_filled.testnet.json`). Non-generic,
/// so the type params live nowhere — an exact type-string match suffices.
#[derive(Debug, Deserialize)]
struct RawOrderFilled {
    pool_id: ObjectId,
    #[allow(dead_code)]
    maker_order_id: u128,
    #[allow(dead_code)]
    taker_order_id: u128,
    #[allow(dead_code)]
    maker_client_order_id: u64,
    #[allow(dead_code)]
    taker_client_order_id: u64,
    price: u64,
    taker_is_bid: bool,
    taker_fee: u64,
    taker_fee_is_deep: bool,
    maker_fee: u64,
    maker_fee_is_deep: bool,
    base_quantity: u64,
    quote_quantity: u64,
    maker_balance_manager_id: ObjectId,
    taker_balance_manager_id: ObjectId,
    timestamp: u64,
}

/// Try to parse `type_str` + `contents` as a DeepBook `OrderFilled` event.
/// `Ok(None)` if the type doesn't match (or DeepBook isn't configured);
/// `Err` if it matches but the BCS payload is malformed.
pub fn parse_deepbook_order_filled(
    types: &EventTypes,
    type_str: &str,
    contents: &[u8],
) -> Result<Option<DeepBookOrderFilledPartial>> {
    let Some(expected) = types.deepbook_order_filled.as_deref() else {
        return Ok(None);
    };
    if type_str != expected {
        return Ok(None);
    }
    let raw: RawOrderFilled = bcs::from_bytes(contents).with_context(|| {
        format!("bcs decode of DeepBook OrderFilled ({} bytes)", contents.len())
    })?;
    Ok(Some(DeepBookOrderFilledPartial {
        pool_id: raw.pool_id,
        taker_balance_manager_id: raw.taker_balance_manager_id,
        maker_balance_manager_id: raw.maker_balance_manager_id,
        taker_is_bid: raw.taker_is_bid,
        base_quantity: raw.base_quantity,
        quote_quantity: raw.quote_quantity,
        price: raw.price,
        taker_fee: raw.taker_fee,
        taker_fee_is_deep: raw.taker_fee_is_deep,
        maker_fee: raw.maker_fee,
        maker_fee_is_deep: raw.maker_fee_is_deep,
        timestamp_ms: raw.timestamp,
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
    const AUCTION_PKG: &str = "0xa1";
    const RFQ_PKG: &str = "0xf1";
    const VAULT_PKG: &str = "0xe1";
    const DEEPBOOK_ORIG: &str =
        "0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982";

    fn pkgs() -> PackageIds<'static> {
        PackageIds { core: PKG, auction: AUCTION_PKG, rfq: RFQ_PKG, vault: VAULT_PKG }
    }

    fn types() -> EventTypes {
        EventTypes::for_packages(pkgs(), Some(DEEPBOOK_ORIG))
    }

    #[test]
    fn type_strings_match_move_module_paths_per_package() {
        let t = types();
        assert_eq!(t.bucket_created, format!("{PKG}::events::BucketCreated"));
        assert_eq!(t.write_executed, format!("{PKG}::events::WriteExecuted"));
        assert_eq!(t.account_deposit, format!("{PKG}::events::AccountDeposit"));
        assert_eq!(
            t.auction_created,
            format!("{AUCTION_PKG}::events::AuctionCreated")
        );
        assert_eq!(t.rfq_created, format!("{RFQ_PKG}::events::RfqCreated"));
        assert_eq!(t.put_rfq_settled, format!("{RFQ_PKG}::events::PutRfqSettled"));
        assert_eq!(t.vault_created, format!("{VAULT_PKG}::events::VaultCreated"));
        assert_eq!(
            t.vault_rfq_settled,
            format!("{VAULT_PKG}::events::VaultRfqSettled")
        );
        assert_eq!(
            t.swap_rfq_settled,
            format!("{VAULT_PKG}::events::SwapRfqSettled")
        );
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
    fn dispatch_decodes_auction_created() {
        let t = types();
        let evt = AuctionCreated {
            auction_id: ObjectId::new([0xaa; 32]),
            origin: ObjectId::new([0xf0; 32]),
            escrow_type: AssetType::new("9b::tbtc::TBTC"),
            bid_type: AssetType::new("9b::tusdc::TUSDC"),
            amount: 250_000_000,
            reserve_bid: 47_619_000,
            deadline_ms: 1_700_000_900_000,
            max_deadline_ms: 1_700_001_500_000,
            min_increment_bps: 100,
            coupled: true,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.auction_created, &bytes).unwrap() {
            Some(ChainEvent::AuctionCreated(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected AuctionCreated, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_rfq_created() {
        let t = types();
        let evt = RfqCreated {
            rfq_id: ObjectId::new([0xaa; 32]),
            auction_id: ObjectId::new([0xac; 32]),
            bucket_id: ObjectId::new([0xb1; 32]),
            origin: ObjectId::new([0xf0; 32]),
            amount: 250_000_000,
            reserve_premium: 47_619_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.rfq_created, &bytes).unwrap() {
            Some(ChainEvent::RfqCreated(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected RfqCreated, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_vault_rfq_settled() {
        let t = types();
        let evt = VaultRfqSettled {
            auction_id: ObjectId::new([0xac; 32]),
            bucket_id: ObjectId::new([0xb1; 32]),
            vault_id: ObjectId::new([0xf0; 32]),
            round: 3,
            winner: SuiAddress::new([0x01; 32]),
            call_recipient: SuiAddress::new([0x02; 32]),
            position_id: ObjectId::new([0x99; 32]),
            amount: 250_000_000,
            gross_premium: 51_000_000,
            fee: 510_000,
            net_premium: 50_490_000,
            range_start: 0,
            range_end: 250_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.vault_rfq_settled, &bytes).unwrap() {
            Some(ChainEvent::VaultRfqSettled(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected VaultRfqSettled, got {other:?}"),
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
        let no_db = EventTypes::for_packages(pkgs(), None);
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
