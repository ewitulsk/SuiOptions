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
    AuctionBid, AuctionCreated, AuctionSettled,
    AuctionUnfilled, BucketCleaned, BucketCreated, BucketInvalidated, BucketRevalidated,
    ChainEvent, CollateralizedWrite, EquityPosted, Exercised, ExpiredOptionBurned, FeeUpdated,
    InstantWithdraw, OffsetClosed, OptionMarketListed,
    PutBucketCleaned, PutBucketCreated, PutBucketInvalidated, PutBucketRevalidated,
    PutCollateralizedWrite, PutExercised, PutExpiredOptionBurned, PutRedeemed, PutRfqCreated,
    PutRfqExpiredUnsold, PutRfqSettled, PutSpreadClosed, PutSpreadExercised, PutSpreadRedeemed,
    PutSpreadWritten, PutWriteExecuted, Redeemed, RfqCreated, RfqExpiredUnsold,
    RfqSettled, SharesClaimed, SignerCreated, SigningKeyRotated, SpreadClosed, SpreadRedeemed,
    SpreadUnwound, SpreadWritten, SwapRfqSettled, SwapRfqUnfilled, VolPosted,
    TreasuryWithdrawn, VaultBucketSelected, VaultConfigUpdated, VaultCreated, VaultDeposit,
    VaultDepositsPaused, VaultFeesCharged, VaultPositionRedeemed, VaultRfqSettled, VaultRfqUnsold,
    VaultRoundFinalized, WithdrawCompleted, WithdrawInitiated, WriteExecuted,
    TvVaultCreated, TvVaultClosing, TvVaultClosed, TvDepositsPaused, TvMmReleaseToggled,
    TvCuratorRotated, TvDeposited, TvWithdrawRequested, TvWithdrawFulfilled, TvSessionSettled,
    TvDepositAssetAdded, TvDepositAssetRemoved, TvHaircutsSet, TvPayoutAssetAmended,
    TvPositionStored, TvPositionRemoved, TvPositionAppraised, TvVaultAppraised,
    TvAdapterAllowed, TvAdapterDisallowed, TvOracleAllowed,
    TvOracleDisallowed, TvProtocolConfigUpdated, TvCollateralReleased, TvCustodyCreated,
    TvExchangeCustodyCreated, TvVaultQuoteFilled, TvQuoteAdapterAdded, TvQuoteAdapterRemoved,
    TvPoolAllowed, TvPoolDisallowed, TvRfqOpened, TvRfqSettled, TvPositionRedeemed,
    TvMmCoinExercised, TvMmOffsetClosed, TvMmCoinReleased, TvTakerSwapExecuted,
    TvBidPlaced, TvBidReclaimed, TvBidRedeemed,
    TvExternalAccountSet, TvExternalAccountCleared, TvExternalReleased, TvExternalReturned,
};
use protocol_types::ids::{ObjectId, SuiAddress};

const EVENTS_MODULE: &str = "events";

/// The published package ids the protocol's events resolve to. `core`,
/// Only `core` is required; the rest are optional and simply don't
/// subscribe when absent.
#[derive(Debug, Clone, Copy)]
pub struct PackageIds<'a> {
    /// options_core.
    pub core: &'a str,
    /// Generic auction venue. Optional since the venue's retirement: the
    /// package is no longer published (see contracts/.deprecated/auction),
    /// so its event families simply don't subscribe.
    pub auction: Option<&'a str>,
    /// options_rfq adapter. Optional, retired with the auction venue
    /// (see contracts/.deprecated/rfq).
    pub rfq: Option<&'a str>,
    /// options_vault. Optional since SO-332: the covered-call vault product
    /// is deprecated and the package is no longer published, so its event
    /// families simply don't subscribe (same posture as `trading_vault`).
    pub vault: Option<&'a str>,
    /// trading_vault (curated vaults, SO-282). Optional: absent on
    /// deployments predating the product; its event families simply
    /// don't subscribe.
    pub trading_vault: Option<&'a str>,
    /// deepbook_adapter (SO-284), optional like `trading_vault`.
    pub deepbook_adapter: Option<&'a str>,
    /// options_adapter (SO-285), optional like `trading_vault`.
    pub options_adapter: Option<&'a str>,
    /// exchange_adapter (SO-370), optional like `trading_vault`.
    pub exchange_adapter: Option<&'a str>,
    /// equity_oracle (SO-299), optional like `trading_vault`.
    pub equity_oracle: Option<&'a str>,
    /// exchange_listing (SO-415/416), optional like `trading_vault`.
    pub exchange_listing: Option<&'a str>,
    /// exchange (in-house orderbook, SO-416), optional like `trading_vault`.
    pub exchange: Option<&'a str>,
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
    pub signer_created: String,
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
    // Offset closes + spreads (options_core, SO-299).
    pub offset_closed: String,
    pub spread_written: String,
    pub spread_unwound: String,
    pub spread_closed: String,
    pub spread_redeemed: String,
    /// Prefix of DeepBook's generic `pool::PoolCreated<Base, Quote>` event
    /// (SO-152). Built from DeepBook's ORIGINAL package id — Sui resolves
    /// event/struct types to the first publish, not the upgraded package
    /// that calls target. `None` on networks without a DeepBook deployment.
    pub deepbook_pool_created_prefix: Option<String>,
    /// DeepBook's non-generic `order_info::OrderFilled` type string (SO-209).
    /// Exact-matchable (zero type params); `None` without a DeepBook deploy.
    pub deepbook_order_filled: Option<String>,
    // Curated trading vaults (SO-282). Built with an unmatchable
    // placeholder when the packages aren't deployed.
    pub tv_vault_created: String,
    pub tv_vault_closing: String,
    pub tv_vault_closed: String,
    pub tv_deposits_paused: String,
    pub tv_mm_release_toggled: String,
    pub tv_curator_rotated: String,
    pub tv_deposited: String,
    pub tv_withdraw_requested: String,
    pub tv_withdraw_fulfilled: String,
    // Multi-asset deposits/withdrawals (SO-370).
    pub tv_deposit_asset_added: String,
    pub tv_deposit_asset_removed: String,
    pub tv_haircuts_set: String,
    pub tv_payout_asset_amended: String,
    pub tv_session_settled: String,
    pub tv_position_stored: String,
    pub tv_position_removed: String,
    // Per-position marks + consumed-appraisal NAV (SO-304).
    pub tv_position_appraised: String,
    pub tv_vault_appraised: String,
    pub tv_adapter_allowed: String,
    pub tv_adapter_disallowed: String,
    pub tv_oracle_allowed: String,
    pub tv_oracle_disallowed: String,
    pub tv_protocol_config_updated: String,
    pub tv_collateral_released: String,
    pub tv_custody_created: String,
    /// exchange_adapter's CustodyCreated (SO-370).
    pub tv_exchange_custody_created: String,
    /// exchange_adapter's VaultQuoteFilled (SO-372: direct vault escrow).
    pub tv_vault_quote_filled: String,
    /// Curator opt-in/out of quote sessions (SO-372).
    pub tv_quote_adapter_added: String,
    pub tv_quote_adapter_removed: String,
    pub tv_pool_allowed: String,
    pub tv_pool_disallowed: String,
    pub tv_rfq_opened: String,
    pub tv_rfq_settled: String,
    pub tv_position_redeemed: String,
    // Trading-vault mm-desk custody ops + vault-funded bids (SO-299).
    pub tv_mm_coin_exercised: String,
    pub tv_mm_offset_closed: String,
    pub tv_mm_coin_released: String,
    pub tv_taker_swap_executed: String,
    pub tv_bid_placed: String,
    pub tv_bid_reclaimed: String,
    pub tv_bid_redeemed: String,
    // Trading-vault external accounts + equity oracle (SO-299).
    pub tv_external_account_set: String,
    pub tv_external_account_cleared: String,
    pub tv_external_released: String,
    pub tv_external_returned: String,
    pub equity_posted: String,
    // Put-side spread compression + vol book (SO-301).
    pub put_spread_written: String,
    pub put_spread_exercised: String,
    pub put_spread_closed: String,
    pub put_spread_redeemed: String,
    pub vol_posted: String,
    // In-house exchange secondary market (SO-416).
    pub option_market_listed: String,
    /// The exchange's `settlement::FillEvent`. Not dispatched to a
    /// `ChainEvent` directly — the worker resolves registry → bucket via
    /// [`parse_exchange_fill`] and promotes or drops it.
    pub exchange_fill: String,
}

impl EventTypes {
    pub fn for_packages(pkgs: PackageIds<'_>, deepbook_original_package_id: Option<&str>) -> Self {
        let core = |name: &str| format!("{}::{EVENTS_MODULE}::{name}", pkgs.core);
        // Retired auction/options_rfq venue: same "unset" placeholder the
        // deprecated-vault families use — it never matches a real on-chain
        // type, so the variants stay in `dispatch` but can never fire.
        let auction = |name: &str| match pkgs.auction {
            Some(pkg) => format!("{pkg}::{EVENTS_MODULE}::{name}"),
            None => format!("unset::{EVENTS_MODULE}::{name}"),
        };
        let rfq = |name: &str| match pkgs.rfq {
            Some(pkg) => format!("{pkg}::{EVENTS_MODULE}::{name}"),
            None => format!("unset::{EVENTS_MODULE}::{name}"),
        };
        // Deprecated covered-call vault (SO-332): same "unset" placeholder the
        // trading-vault families use — it never matches a real on-chain type,
        // so the variants stay in `dispatch` but can never fire.
        let vault = |name: &str| match pkgs.vault {
            Some(pkg) => format!("{pkg}::{EVENTS_MODULE}::{name}"),
            None => format!("unset::{EVENTS_MODULE}::{name}"),
        };
        // Trading-vault families: an "unset" placeholder never matches a
        // real on-chain type string.
        let tv = |name: &str| match pkgs.trading_vault {
            Some(pkg) => format!("{pkg}::{EVENTS_MODULE}::{name}"),
            None => format!("unset::{EVENTS_MODULE}::{name}"),
        };
        let tv_mm = |name: &str| match pkgs.trading_vault {
            Some(pkg) => format!("{pkg}::vault_mm::{name}"),
            None => format!("unset::vault_mm::{name}"),
        };
        let dba = |name: &str| match pkgs.deepbook_adapter {
            Some(pkg) => format!("{pkg}::deepbook_adapter::{name}"),
            None => format!("unset::deepbook_adapter::{name}"),
        };
        let oa = |name: &str| match pkgs.options_adapter {
            Some(pkg) => format!("{pkg}::options_adapter::{name}"),
            None => format!("unset::options_adapter::{name}"),
        };
        let ea = |name: &str| match pkgs.exchange_adapter {
            Some(pkg) => format!("{pkg}::exchange_adapter::{name}"),
            None => format!("unset::exchange_adapter::{name}"),
        };
        let eo = |name: &str| match pkgs.equity_oracle {
            Some(pkg) => format!("{pkg}::equity_oracle::{name}"),
            None => format!("unset::equity_oracle::{name}"),
        };
        let vb = |name: &str| match pkgs.options_adapter {
            Some(pkg) => format!("{pkg}::vol_book::{name}"),
            None => format!("unset::vol_book::{name}"),
        };
        let el = |name: &str| match pkgs.exchange_listing {
            Some(pkg) => format!("{pkg}::exchange_listing::{name}"),
            None => format!("unset::exchange_listing::{name}"),
        };
        let exch = |name: &str| match pkgs.exchange {
            Some(pkg) => format!("{pkg}::settlement::{name}"),
            None => format!("unset::settlement::{name}"),
        };
        Self {
            bucket_created: core("BucketCreated"),
            write_executed: core("WriteExecuted"),
            exercised: core("Exercised"),
            redeemed: core("Redeemed"),
            expired_option_burned: core("ExpiredOptionBurned"),
            bucket_cleaned: core("BucketCleaned"),
            bucket_invalidated: core("BucketInvalidated"),
            bucket_revalidated: core("BucketRevalidated"),
            signer_created: core("SignerCreated"),
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
            offset_closed: core("OffsetClosed"),
            spread_written: core("SpreadWritten"),
            spread_unwound: core("SpreadUnwound"),
            spread_closed: core("SpreadClosed"),
            spread_redeemed: core("SpreadRedeemed"),
            deepbook_pool_created_prefix: deepbook_original_package_id
                .map(|pkg| format!("{pkg}::pool::PoolCreated<")),
            deepbook_order_filled: deepbook_original_package_id
                .map(|pkg| format!("{pkg}::order_info::OrderFilled")),
            tv_vault_created: tv("VaultCreated"),
            tv_vault_closing: tv("VaultClosing"),
            tv_vault_closed: tv("VaultClosed"),
            tv_deposits_paused: tv("DepositsPaused"),
            tv_mm_release_toggled: tv("MmReleaseToggled"),
            tv_curator_rotated: tv("CuratorRotated"),
            tv_deposited: tv("Deposited"),
            tv_withdraw_requested: tv("WithdrawRequested"),
            tv_withdraw_fulfilled: tv("WithdrawFulfilled"),
            tv_deposit_asset_added: tv("DepositAssetAdded"),
            tv_deposit_asset_removed: tv("DepositAssetRemoved"),
            tv_haircuts_set: tv("HaircutsSet"),
            tv_payout_asset_amended: tv("PayoutAssetAmended"),
            tv_session_settled: tv("SessionSettled"),
            tv_position_stored: tv("PositionStored"),
            tv_position_removed: tv("PositionRemoved"),
            tv_position_appraised: tv("PositionAppraised"),
            tv_vault_appraised: tv("VaultAppraised"),
            tv_adapter_allowed: tv("AdapterAllowed"),
            tv_adapter_disallowed: tv("AdapterDisallowed"),
            tv_oracle_allowed: tv("OracleAllowed"),
            tv_oracle_disallowed: tv("OracleDisallowed"),
            tv_protocol_config_updated: tv("ProtocolConfigUpdated"),
            tv_collateral_released: tv_mm("CollateralReleased"),
            tv_custody_created: dba("CustodyCreated"),
            tv_exchange_custody_created: ea("CustodyCreated"),
            tv_vault_quote_filled: ea("VaultQuoteFilled"),
            tv_quote_adapter_added: tv("QuoteAdapterAdded"),
            tv_quote_adapter_removed: tv("QuoteAdapterRemoved"),
            tv_pool_allowed: dba("PoolAllowed"),
            tv_pool_disallowed: dba("PoolDisallowed"),
            tv_rfq_opened: oa("RfqOpened"),
            tv_rfq_settled: oa("RfqSettled"),
            tv_position_redeemed: oa("PositionRedeemed"),
            // Mm* structs are DEFINED in trading_vault::events (vault_mm
            // only calls the emitters), so they resolve via `tv`, not
            // `tv_mm`.
            tv_mm_coin_exercised: tv("MmCoinExercised"),
            tv_mm_offset_closed: tv("MmOffsetClosed"),
            tv_mm_coin_released: tv("MmCoinReleased"),
            tv_taker_swap_executed: dba("TakerSwapExecuted"),
            tv_bid_placed: oa("BidPlaced"),
            tv_bid_reclaimed: oa("BidReclaimed"),
            tv_bid_redeemed: oa("BidRedeemed"),
            tv_external_account_set: tv("ExternalAccountSet"),
            tv_external_account_cleared: tv("ExternalAccountCleared"),
            tv_external_released: tv("ExternalReleased"),
            tv_external_returned: tv("ExternalReturned"),
            equity_posted: eo("EquityPosted"),
            put_spread_written: core("PutSpreadWritten"),
            put_spread_exercised: core("PutSpreadExercised"),
            put_spread_closed: core("PutSpreadClosed"),
            put_spread_redeemed: core("PutSpreadRedeemed"),
            vol_posted: vb("VolPosted"),
            option_market_listed: el("OptionMarketListed"),
            exchange_fill: exch("FillEvent"),
        }
    }

    pub fn all_strings(&self) -> [&str; 107] {
        [
            &self.bucket_created,
            &self.write_executed,
            &self.exercised,
            &self.redeemed,
            &self.expired_option_burned,
            &self.bucket_cleaned,
            &self.bucket_invalidated,
            &self.bucket_revalidated,
            &self.signer_created,
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
            &self.offset_closed,
            &self.spread_written,
            &self.spread_unwound,
            &self.spread_closed,
            &self.spread_redeemed,
            &self.tv_vault_created,
            &self.tv_vault_closing,
            &self.tv_vault_closed,
            &self.tv_deposits_paused,
            &self.tv_mm_release_toggled,
            &self.tv_curator_rotated,
            &self.tv_deposited,
            &self.tv_withdraw_requested,
            &self.tv_withdraw_fulfilled,
            &self.tv_deposit_asset_added,
            &self.tv_deposit_asset_removed,
            &self.tv_haircuts_set,
            &self.tv_payout_asset_amended,
            &self.tv_session_settled,
            &self.tv_position_stored,
            &self.tv_position_removed,
            &self.tv_position_appraised,
            &self.tv_vault_appraised,
            &self.tv_adapter_allowed,
            &self.tv_adapter_disallowed,
            &self.tv_oracle_allowed,
            &self.tv_oracle_disallowed,
            &self.tv_protocol_config_updated,
            &self.tv_collateral_released,
            &self.tv_custody_created,
            &self.tv_exchange_custody_created,
            &self.tv_vault_quote_filled,
            &self.tv_quote_adapter_added,
            &self.tv_quote_adapter_removed,
            &self.tv_pool_allowed,
            &self.tv_pool_disallowed,
            &self.tv_rfq_opened,
            &self.tv_rfq_settled,
            &self.tv_position_redeemed,
            &self.tv_mm_coin_exercised,
            &self.tv_mm_offset_closed,
            &self.tv_mm_coin_released,
            &self.tv_taker_swap_executed,
            &self.tv_bid_placed,
            &self.tv_bid_reclaimed,
            &self.tv_bid_redeemed,
            &self.tv_external_account_set,
            &self.tv_external_account_cleared,
            &self.tv_external_released,
            &self.tv_external_returned,
            &self.equity_posted,
            &self.put_spread_written,
            &self.put_spread_exercised,
            &self.put_spread_closed,
            &self.put_spread_redeemed,
            &self.vol_posted,
            &self.option_market_listed,
            &self.exchange_fill,
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
    } else if type_str == types.signer_created {
        decode!(SignerCreated, SignerCreated)
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
    } else if type_str == types.offset_closed {
        decode!(OffsetClosed, OffsetClosed)
    } else if type_str == types.spread_written {
        decode!(SpreadWritten, SpreadWritten)
    } else if type_str == types.spread_unwound {
        decode!(SpreadUnwound, SpreadUnwound)
    } else if type_str == types.spread_closed {
        decode!(SpreadClosed, SpreadClosed)
    } else if type_str == types.spread_redeemed {
        decode!(SpreadRedeemed, SpreadRedeemed)
    } else if type_str == types.put_spread_written {
        decode!(PutSpreadWritten, PutSpreadWritten)
    } else if type_str == types.put_spread_exercised {
        decode!(PutSpreadExercised, PutSpreadExercised)
    } else if type_str == types.put_spread_closed {
        decode!(PutSpreadClosed, PutSpreadClosed)
    } else if type_str == types.put_spread_redeemed {
        decode!(PutSpreadRedeemed, PutSpreadRedeemed)
    } else if type_str == types.vol_posted {
        decode!(VolPosted, VolPosted)
    } else if type_str == types.tv_vault_created {
        decode!(TvVaultCreated, TvVaultCreated)
    } else if type_str == types.tv_vault_closing {
        decode!(TvVaultClosing, TvVaultClosing)
    } else if type_str == types.tv_vault_closed {
        decode!(TvVaultClosed, TvVaultClosed)
    } else if type_str == types.tv_deposits_paused {
        decode!(TvDepositsPaused, TvDepositsPaused)
    } else if type_str == types.tv_mm_release_toggled {
        decode!(TvMmReleaseToggled, TvMmReleaseToggled)
    } else if type_str == types.tv_curator_rotated {
        decode!(TvCuratorRotated, TvCuratorRotated)
    } else if type_str == types.tv_deposited {
        decode!(TvDeposited, TvDeposited)
    } else if type_str == types.tv_withdraw_requested {
        decode!(TvWithdrawRequested, TvWithdrawRequested)
    } else if type_str == types.tv_withdraw_fulfilled {
        decode!(TvWithdrawFulfilled, TvWithdrawFulfilled)
    } else if type_str == types.tv_deposit_asset_added {
        decode!(TvDepositAssetAdded, TvDepositAssetAdded)
    } else if type_str == types.tv_deposit_asset_removed {
        decode!(TvDepositAssetRemoved, TvDepositAssetRemoved)
    } else if type_str == types.tv_haircuts_set {
        decode!(TvHaircutsSet, TvHaircutsSet)
    } else if type_str == types.tv_payout_asset_amended {
        decode!(TvPayoutAssetAmended, TvPayoutAssetAmended)
    } else if type_str == types.tv_session_settled {
        decode!(TvSessionSettled, TvSessionSettled)
    } else if type_str == types.tv_position_stored {
        decode!(TvPositionStored, TvPositionStored)
    } else if type_str == types.tv_position_removed {
        decode!(TvPositionRemoved, TvPositionRemoved)
    } else if type_str == types.tv_position_appraised {
        decode!(TvPositionAppraised, TvPositionAppraised)
    } else if type_str == types.tv_vault_appraised {
        decode!(TvVaultAppraised, TvVaultAppraised)
    } else if type_str == types.tv_adapter_allowed {
        decode!(TvAdapterAllowed, TvAdapterAllowed)
    } else if type_str == types.tv_adapter_disallowed {
        decode!(TvAdapterDisallowed, TvAdapterDisallowed)
    } else if type_str == types.tv_oracle_allowed {
        decode!(TvOracleAllowed, TvOracleAllowed)
    } else if type_str == types.tv_oracle_disallowed {
        decode!(TvOracleDisallowed, TvOracleDisallowed)
    } else if type_str == types.tv_protocol_config_updated {
        decode!(TvProtocolConfigUpdated, TvProtocolConfigUpdated)
    } else if type_str == types.tv_collateral_released {
        decode!(TvCollateralReleased, TvCollateralReleased)
    } else if type_str == types.tv_custody_created {
        decode!(TvCustodyCreated, TvCustodyCreated)
    } else if type_str == types.tv_exchange_custody_created {
        decode!(TvExchangeCustodyCreated, TvExchangeCustodyCreated)
    } else if type_str == types.tv_vault_quote_filled {
        decode!(TvVaultQuoteFilled, TvVaultQuoteFilled)
    } else if type_str == types.tv_quote_adapter_added {
        decode!(TvQuoteAdapterAdded, TvQuoteAdapterAdded)
    } else if type_str == types.tv_quote_adapter_removed {
        decode!(TvQuoteAdapterRemoved, TvQuoteAdapterRemoved)
    } else if type_str == types.tv_pool_allowed {
        decode!(TvPoolAllowed, TvPoolAllowed)
    } else if type_str == types.tv_pool_disallowed {
        decode!(TvPoolDisallowed, TvPoolDisallowed)
    } else if type_str == types.tv_rfq_opened {
        decode!(TvRfqOpened, TvRfqOpened)
    } else if type_str == types.tv_rfq_settled {
        decode!(TvRfqSettled, TvRfqSettled)
    } else if type_str == types.tv_position_redeemed {
        decode!(TvPositionRedeemed, TvPositionRedeemed)
    } else if type_str == types.tv_mm_coin_exercised {
        decode!(TvMmCoinExercised, TvMmCoinExercised)
    } else if type_str == types.tv_mm_offset_closed {
        decode!(TvMmOffsetClosed, TvMmOffsetClosed)
    } else if type_str == types.tv_mm_coin_released {
        decode!(TvMmCoinReleased, TvMmCoinReleased)
    } else if type_str == types.tv_taker_swap_executed {
        decode!(TvTakerSwapExecuted, TvTakerSwapExecuted)
    } else if type_str == types.tv_bid_placed {
        decode!(TvBidPlaced, TvBidPlaced)
    } else if type_str == types.tv_bid_reclaimed {
        decode!(TvBidReclaimed, TvBidReclaimed)
    } else if type_str == types.tv_bid_redeemed {
        decode!(TvBidRedeemed, TvBidRedeemed)
    } else if type_str == types.tv_external_account_set {
        decode!(TvExternalAccountSet, TvExternalAccountSet)
    } else if type_str == types.tv_external_account_cleared {
        decode!(TvExternalAccountCleared, TvExternalAccountCleared)
    } else if type_str == types.tv_external_released {
        decode!(TvExternalReleased, TvExternalReleased)
    } else if type_str == types.tv_external_returned {
        decode!(TvExternalReturned, TvExternalReturned)
    } else if type_str == types.equity_posted {
        decode!(EquityPosted, EquityPosted)
    } else if type_str == types.option_market_listed {
        decode!(OptionMarketListed, OptionMarketListed)
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

/// Exchange `settlement::FillEvent` decoded but not yet tied to a bucket
/// (SO-416). The worker resolves `registry` against known option-market
/// listings and either promotes it into `ChainEvent::ExchangeOptionFill` or
/// drops it (a fill on a spot market).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeFillPartial {
    pub registry: ObjectId,
    pub digest: Vec<u8>,
    pub maker: SuiAddress,
    pub taker: SuiAddress,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub maker_fee_bps: u64,
    pub taker_fee_bps: u64,
    pub maker_fee: u64,
    pub taker_fee: u64,
    pub maker_sold_base: bool,
    pub taker_token_filled_total: u64,
    pub timestamp_ms: u64,
}

/// Raw BCS mirror of the exchange's `settlement::FillEvent` payload. Field
/// order matches `contracts/exchange/sources/settlement.move`. Non-generic,
/// so an exact type-string match suffices.
#[derive(Debug, Deserialize)]
struct RawExchangeFill {
    registry: ObjectId,
    digest: Vec<u8>,
    maker: SuiAddress,
    taker: SuiAddress,
    base_amount: u64,
    quote_amount: u64,
    maker_fee_bps: u64,
    taker_fee_bps: u64,
    maker_fee: u64,
    taker_fee: u64,
    maker_sold_base: bool,
    taker_token_filled_total: u64,
    timestamp_ms: u64,
}

/// Try to parse `type_str` + `contents` as an exchange `FillEvent`.
/// `Ok(None)` if the type doesn't match; `Err` if it matches but the BCS
/// payload is malformed.
pub fn parse_exchange_fill(
    types: &EventTypes,
    type_str: &str,
    contents: &[u8],
) -> Result<Option<ExchangeFillPartial>> {
    if type_str != types.exchange_fill {
        return Ok(None);
    }
    let raw: RawExchangeFill = bcs::from_bytes(contents).with_context(|| {
        format!("bcs decode of exchange FillEvent ({} bytes)", contents.len())
    })?;
    Ok(Some(ExchangeFillPartial {
        registry: raw.registry,
        digest: raw.digest,
        maker: raw.maker,
        taker: raw.taker,
        base_amount: raw.base_amount,
        quote_amount: raw.quote_amount,
        maker_fee_bps: raw.maker_fee_bps,
        taker_fee_bps: raw.taker_fee_bps,
        maker_fee: raw.maker_fee,
        taker_fee: raw.taker_fee,
        maker_sold_base: raw.maker_sold_base,
        taker_token_filled_total: raw.taker_token_filled_total,
        timestamp_ms: raw.timestamp_ms,
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
    const TV_PKG: &str = "0xt1";
    const EQUITY_ORACLE_PKG: &str = "0xeq1";
    const DEEPBOOK_ORIG: &str =
        "0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982";

    fn pkgs() -> PackageIds<'static> {
        PackageIds { core: PKG, auction: Some(AUCTION_PKG), rfq: Some(RFQ_PKG), vault: Some(VAULT_PKG), trading_vault: Some(TV_PKG), deepbook_adapter: None, options_adapter: None, exchange_adapter: None, equity_oracle: Some(EQUITY_ORACLE_PKG), exchange_listing: None, exchange: None }
    }

    fn types() -> EventTypes {
        EventTypes::for_packages(pkgs(), Some(DEEPBOOK_ORIG))
    }

    #[test]
    fn type_strings_match_move_module_paths_per_package() {
        let t = types();
        assert_eq!(t.bucket_created, format!("{PKG}::events::BucketCreated"));
        assert_eq!(t.write_executed, format!("{PKG}::events::WriteExecuted"));
        assert_eq!(t.signer_created, format!("{PKG}::events::SignerCreated"));
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
        assert_eq!(t.offset_closed, format!("{PKG}::events::OffsetClosed"));
        assert_eq!(t.spread_written, format!("{PKG}::events::SpreadWritten"));
        assert_eq!(
            t.tv_external_account_set,
            format!("{TV_PKG}::events::ExternalAccountSet")
        );
        // The equity oracle's event lives in its own module, not `events`.
        assert_eq!(
            t.equity_posted,
            format!("{EQUITY_ORACLE_PKG}::equity_oracle::EquityPosted")
        );
        // Absent family → unmatchable placeholder.
        let no_eo = EventTypes::for_packages(
            PackageIds { equity_oracle: None, ..pkgs() },
            Some(DEEPBOOK_ORIG),
        );
        assert_eq!(no_eo.equity_posted, "unset::equity_oracle::EquityPosted");
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
            signer_id: ObjectId::new([0x22; 32]),
            collateral_source: ObjectId::new([0x23; 32]),
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
    fn dispatch_decodes_offset_closed() {
        let t = types();
        let evt = OffsetClosed {
            bucket_id: ObjectId::new([0xb1; 32]),
            closer: SuiAddress::new([0x01; 32]),
            position_id: ObjectId::new([0x99; 32]),
            is_put: false,
            amount: 250_000_000,
            collateral_returned: 250_000_000,
            range_start: 0,
            range_end: 250_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.offset_closed, &bytes).unwrap() {
            Some(ChainEvent::OffsetClosed(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected OffsetClosed, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_spread_written() {
        let t = types();
        let evt = SpreadWritten {
            bucket_id: ObjectId::new([0xb1; 32]),
            long_bucket_id: ObjectId::new([0xb2; 32]),
            writer: SuiAddress::new([0x01; 32]),
            position_id: ObjectId::new([0x99; 32]),
            amount: 250_000_000,
            exercise_cash: 12_500_000,
            range_start: 250_000_000,
            range_end: 500_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.spread_written, &bytes).unwrap() {
            Some(ChainEvent::SpreadWritten(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected SpreadWritten, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_tv_external_released() {
        let t = types();
        let evt = TvExternalReleased {
            vault_id: ObjectId::new([0xf0; 32]),
            account: SuiAddress::new([0x1a; 32]),
            amount: 25_000_000,
            exposure: 75_000_000,
            nav: 1_000_000_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.tv_external_released, &bytes).unwrap() {
            Some(ChainEvent::TvExternalReleased(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected TvExternalReleased, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_tv_mm_coin_exercised() {
        let t = types();
        let evt = TvMmCoinExercised {
            vault_id: ObjectId::new([0xf0; 32]),
            bucket_id: ObjectId::new([0xb1; 32]),
            coin_position_id: ObjectId::new([0xc1; 32]),
            is_put: false,
            amount: 250_000_000,
            settlement_amount: 12_500_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        // Defined in trading_vault::events, so it must resolve via the
        // trading_vault package's events module.
        assert_eq!(t.tv_mm_coin_exercised, format!("{TV_PKG}::events::MmCoinExercised"));
        match dispatch(&t, &t.tv_mm_coin_exercised, &bytes).unwrap() {
            Some(ChainEvent::TvMmCoinExercised(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected TvMmCoinExercised, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_tv_bid_placed() {
        // The bid events live in the options_adapter package's module.
        let oa_pkg = "0xoa1";
        let t = EventTypes::for_packages(
            PackageIds { options_adapter: Some(oa_pkg), ..pkgs() },
            Some(DEEPBOOK_ORIG),
        );
        let evt = TvBidPlaced {
            vault_id: ObjectId::new([0xf0; 32]),
            ticket_id: ObjectId::new([0x71; 32]),
            auction_id: ObjectId::new([0xac; 32]),
            bucket_id: ObjectId::new([0xb1; 32]),
            escrow_amount: 51_000_000,
            win_type: AssetType::new("9b::call_3::CALL_3"),
            win_amount: 250_000_000,
            is_put: false,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        assert_eq!(t.tv_bid_placed, format!("{oa_pkg}::options_adapter::BidPlaced"));
        match dispatch(&t, &t.tv_bid_placed, &bytes).unwrap() {
            Some(ChainEvent::TvBidPlaced(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected TvBidPlaced, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_exchange_adapter_custody_created() {
        // The exchange-adapter custody event lives in the exchange_adapter
        // package's module (SO-370), like the deepbook-adapter's.
        let ea_pkg = "0xea1";
        let t = EventTypes::for_packages(
            PackageIds { exchange_adapter: Some(ea_pkg), ..pkgs() },
            Some(DEEPBOOK_ORIG),
        );
        let evt = TvExchangeCustodyCreated {
            vault_id: ObjectId::new([0xf0; 32]),
            custody_id: ObjectId::new([0xc1; 32]),
            balance_manager_id: ObjectId::new([0xb2; 32]),
            direct: true,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        assert_eq!(
            t.tv_exchange_custody_created,
            format!("{ea_pkg}::exchange_adapter::CustodyCreated")
        );
        match dispatch(&t, &t.tv_exchange_custody_created, &bytes).unwrap() {
            Some(ChainEvent::TvExchangeCustodyCreated(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected TvExchangeCustodyCreated, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_direct_escrow_events() {
        // SO-372: the fill event rides in the exchange_adapter module; the
        // quote-adapter opt-in/out events in trading_vault::events.
        let ea_pkg = "0xea1";
        let t = EventTypes::for_packages(
            PackageIds { exchange_adapter: Some(ea_pkg), ..pkgs() },
            Some(DEEPBOOK_ORIG),
        );
        let filled = TvVaultQuoteFilled {
            vault_id: ObjectId::new([0xf0; 32]),
            custody_id: ObjectId::new([0xc1; 32]),
            balance_manager_id: ObjectId::new([0xb2; 32]),
            sold_base: true,
            base_amount: 1_000_000,
            quote_amount: 2_000_000,
        };
        let bytes = bcs::to_bytes(&filled).unwrap();
        assert_eq!(
            t.tv_vault_quote_filled,
            format!("{ea_pkg}::exchange_adapter::VaultQuoteFilled")
        );
        match dispatch(&t, &t.tv_vault_quote_filled, &bytes).unwrap() {
            Some(ChainEvent::TvVaultQuoteFilled(decoded)) => assert_eq!(decoded, filled),
            other => panic!("expected TvVaultQuoteFilled, got {other:?}"),
        }
        let added = TvQuoteAdapterAdded {
            vault_id: ObjectId::new([0xf0; 32]),
            adapter: AssetType::new("ea1::exchange_adapter::ExchangeAdapter"),
        };
        let bytes = bcs::to_bytes(&added).unwrap();
        match dispatch(&t, &t.tv_quote_adapter_added, &bytes).unwrap() {
            Some(ChainEvent::TvQuoteAdapterAdded(decoded)) => assert_eq!(decoded, added),
            other => panic!("expected TvQuoteAdapterAdded, got {other:?}"),
        }
        let removed = TvQuoteAdapterRemoved {
            vault_id: ObjectId::new([0xf0; 32]),
            adapter: AssetType::new("ea1::exchange_adapter::ExchangeAdapter"),
        };
        let bytes = bcs::to_bytes(&removed).unwrap();
        match dispatch(&t, &t.tv_quote_adapter_removed, &bytes).unwrap() {
            Some(ChainEvent::TvQuoteAdapterRemoved(decoded)) => assert_eq!(decoded, removed),
            other => panic!("expected TvQuoteAdapterRemoved, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_multi_asset_deposit_events() {
        let t = types();
        assert_eq!(
            t.tv_deposit_asset_added,
            format!("{TV_PKG}::events::DepositAssetAdded")
        );
        assert_eq!(
            t.tv_payout_asset_amended,
            format!("{TV_PKG}::events::PayoutAssetAmended")
        );
        let added = TvDepositAssetAdded {
            vault_id: ObjectId::new([0xf0; 32]),
            asset: AssetType::new("9b::tbtc::TBTC"),
        };
        let bytes = bcs::to_bytes(&added).unwrap();
        match dispatch(&t, &t.tv_deposit_asset_added, &bytes).unwrap() {
            Some(ChainEvent::TvDepositAssetAdded(decoded)) => assert_eq!(decoded, added),
            other => panic!("expected TvDepositAssetAdded, got {other:?}"),
        }
        let amended = TvPayoutAssetAmended {
            vault_id: ObjectId::new([0xf0; 32]),
            seq: 3,
            payout_asset: AssetType::new("9b::tbtc::TBTC"),
        };
        let bytes = bcs::to_bytes(&amended).unwrap();
        match dispatch(&t, &t.tv_payout_asset_amended, &bytes).unwrap() {
            Some(ChainEvent::TvPayoutAssetAmended(decoded)) => assert_eq!(decoded, amended),
            other => panic!("expected TvPayoutAssetAmended, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_tv_appraisal_events() {
        let t = types();
        assert_eq!(t.tv_position_appraised, format!("{TV_PKG}::events::PositionAppraised"));
        assert_eq!(t.tv_vault_appraised, format!("{TV_PKG}::events::VaultAppraised"));
        let pos = TvPositionAppraised {
            vault_id: ObjectId::new([0xf0; 32]),
            adapter: AssetType::new("9b::deepbook_adapter::DeepBookAdapter"),
            position_id: ObjectId::new([0x99; 32]),
            value: 1_500_000,
        };
        let bytes = bcs::to_bytes(&pos).unwrap();
        match dispatch(&t, &t.tv_position_appraised, &bytes).unwrap() {
            Some(ChainEvent::TvPositionAppraised(decoded)) => assert_eq!(decoded, pos),
            other => panic!("expected TvPositionAppraised, got {other:?}"),
        }
        let nav = TvVaultAppraised {
            vault_id: ObjectId::new([0xf0; 32]),
            total_value: 2_500_000_000_000,
            position_total: 3,
        };
        let bytes = bcs::to_bytes(&nav).unwrap();
        match dispatch(&t, &t.tv_vault_appraised, &bytes).unwrap() {
            Some(ChainEvent::TvVaultAppraised(decoded)) => assert_eq!(decoded, nav),
            other => panic!("expected TvVaultAppraised, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_decodes_equity_posted() {
        let t = types();
        let evt = EquityPosted {
            vault_id: ObjectId::new([0xf0; 32]),
            poster: SuiAddress::new([0x2b; 32]),
            equity: 80_000_000,
            previous: 75_000_000,
            seeded: true,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        match dispatch(&t, &t.equity_posted, &bytes).unwrap() {
            Some(ChainEvent::EquityPosted(decoded)) => assert_eq!(decoded, evt),
            other => panic!("expected EquityPosted, got {other:?}"),
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

    /// SO-299 regression: a package id with a LEADING ZERO renders
    /// short-form from `Display` (0x909…) and never byte-matches the
    /// padded id token-info serves (0x0909…). The worker must dispatch
    /// on the canonical rendering.
    #[test]
    fn leading_zero_package_dispatches_canonically() {
        let padded = "0x0909ea478c484259b693faad871bf51affbefb78f630364554cbab51eeba0a2e";
        let tag =
            sui_types::parse_sui_struct_tag(&format!("{padded}::events::Deposited")).unwrap();
        // Display strips the zero — the historical bug.
        assert!(tag.to_string().starts_with("0x909ea478"));
        // Canonical keeps it, matching the token-info-built string.
        assert_eq!(
            tag.to_canonical_string(true),
            format!("{padded}::events::Deposited"),
        );
    }
}
