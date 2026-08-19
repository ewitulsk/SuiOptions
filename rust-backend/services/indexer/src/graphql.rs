//! GraphQL query API over the indexer's Postgres views (SO-97).
//!
//! The indexer's outbound query surface, internal-only. Diesel is sync, so
//! resolvers hop onto `spawn_blocking` over the r2d2 pool.
//! Two queries:
//!   - `positions(objectIds)` — enrich the wallet-direct Dashboard list.
//!   - `events(filter, …)`    — generalized event query with a recursive
//!     filter (eventType / participant / payloadContains / ranges / and·or·not).
//! A GraphiQL playground is served at `GET /graphql` for ad-hoc developer use.
//!
//! All on-chain integers are returned as decimal strings to dodge JS/JSON
//! 53-bit precision loss; the caller parses + scales.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_graphql::http::GraphiQLSource;
use async_graphql::{
    Context, EmptyMutation, EmptySubscription, Enum, InputObject, Json, Object, Schema,
    SimpleObject,
};
use async_graphql_axum::GraphQL;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post_service};
use axum::{Extension, Router};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{
    AccountRow, BucketRow, IndexedEventRow, PositionRow, RfqBidRow, RfqRow,
    TradingVaultPositionRow, TradingVaultRow, VaultReceiptRow, VaultRoundRow, VaultRow,
};
use crate::db::{BucketQuery, EventFilter, EventQuery, Repo};
use crate::progress::{ProgressSnapshot, ProgressState};

/// An enriched written position: the on-chain `Position` joined to its bucket,
/// plus the provenance denormalized onto the row at mint.
#[derive(SimpleObject)]
pub struct PositionGql {
    pub object_id: String,
    pub bucket_id: String,
    pub recipient: String,
    pub range_start_raw: String,
    pub range_end_raw: String,
    // bucket
    pub asset_type: String,
    pub settlement_type: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    /// "call" or "put".
    pub option_kind: String,
    // provenance (denormalized from the minting WriteExecuted)
    pub premium_received_raw: String,
    pub mm_account_id: String,
    pub tx_digest: String,
    pub minted_at_ms: String,
}

impl From<(PositionRow, BucketRow)> for PositionGql {
    fn from((p, b): (PositionRow, BucketRow)) -> Self {
        PositionGql {
            object_id: p.object_id,
            bucket_id: p.bucket_id,
            recipient: p.recipient,
            range_start_raw: p.range_start.to_string(),
            range_end_raw: p.range_end.to_string(),
            asset_type: b.asset_type,
            settlement_type: b.settlement_type,
            strike_raw: b.strike.to_string(),
            strike_scale: b.strike_scale as i32,
            expiry_ms: b.expiry_ms.to_string(),
            total_written_raw: b.total_written.to_string(),
            exercise_cursor_raw: b.exercise_cursor.to_string(),
            option_kind: p.option_kind,
            premium_received_raw: p.premium_received.to_string(),
            mm_account_id: p.mm_account_id,
            tx_digest: p.tx_digest,
            minted_at_ms: p.minted_at_ms.to_string(),
        }
    }
}

/// One indexed event. `payload` is the raw event JSON; integers are decimal
/// strings (precision-safe). Filter against it with `EventFilter`.
#[derive(SimpleObject)]
pub struct EventGql {
    pub sequence: String,
    pub checkpoint: String,
    pub tx_digest: String,
    pub event_index: i32,
    pub timestamp_ms: String,
    pub event_type: String,
    pub payload: Json<serde_json::Value>,
}

impl From<IndexedEventRow> for EventGql {
    fn from(r: IndexedEventRow) -> Self {
        EventGql {
            sequence: r.sequence.to_string(),
            checkpoint: r.checkpoint.to_string(),
            tx_digest: r.tx_digest,
            event_index: r.event_index,
            timestamp_ms: r.timestamp_ms.to_string(),
            event_type: r.event_type,
            payload: Json(r.payload),
        }
    }
}

/// One bucket from the materialized view. On-chain integers are decimal
/// strings (precision-safe); the caller parses + scales. Backs the JIT
/// `bucket(id)` / `buckets(...)` queries that replace consumer in-memory
/// bucket mirrors.
#[derive(SimpleObject)]
pub struct BucketGql {
    pub bucket_id: String,
    pub asset_type: String,
    pub settlement_type: String,
    pub call_type: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    pub cleaned: bool,
    pub invalidated: bool,
    /// "call" or "put".
    pub option_kind: String,
    /// DeepBook pool trading this bucket's call coin (SO-152); null until a
    /// venue is created.
    pub deepbook_pool_id: Option<String>,
    /// In-house exchange market (SettlementRegistry) trading this bucket's
    /// option coin (SO-416); null until a market is listed.
    pub exchange_market_id: Option<String>,
}

impl BucketGql {
    fn from_row(
        b: BucketRow,
        deepbook_pool_id: Option<String>,
        exchange_market_id: Option<String>,
    ) -> Self {
        BucketGql {
            bucket_id: b.bucket_id,
            asset_type: b.asset_type,
            settlement_type: b.settlement_type,
            call_type: b.call_type,
            strike_raw: b.strike.to_string(),
            strike_scale: b.strike_scale as i32,
            expiry_ms: b.expiry_ms.to_string(),
            total_written_raw: b.total_written.to_string(),
            exercise_cursor_raw: b.exercise_cursor.to_string(),
            cleaned: b.cleaned,
            invalidated: b.invalidated,
            option_kind: b.option_kind,
            deepbook_pool_id,
            exchange_market_id,
        }
    }
}

/// One QuoteSigner registration (signing key + owner). Backs the JIT
/// `account(id)` query the quoting-service authenticates MMs against. There
/// are NO balances: core holds no MM funds under the collateral abstraction.
/// `signing_pubkey_hex` is lowercase hex (no `0x`); `signing_scheme` is the
/// on-chain u8 tag (0=Ed25519, 1=Secp256k1, 2=Secp256r1), null only for
/// un-backfilled rows.
#[derive(SimpleObject)]
pub struct AccountGql {
    pub account_id: String,
    pub owner: Option<String>,
    pub signing_scheme: Option<i32>,
    pub signing_pubkey_hex: String,
}

impl AccountGql {
    fn build(acct: AccountRow) -> Self {
        AccountGql {
            account_id: acct.account_id,
            owner: acct.owner,
            signing_scheme: acct.signing_scheme.map(|s| s as i32),
            signing_pubkey_hex: hex_encode(&acct.signing_pubkey),
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One on-chain auction (C3, four-package layout). Numeric fields are
/// decimal strings.
#[derive(SimpleObject)]
pub struct RfqGql {
    /// The generic auction object id (rows are keyed by auction now).
    pub rfq_id: String,
    /// The options_rfq adapter's Rfq metadata object id; null for
    /// vault-coupled and swap auctions.
    pub meta_id: Option<String>,
    /// Null for swaps / not-yet-enriched coupled auctions.
    pub bucket_id: Option<String>,
    /// Vault id (coupled auctions) or seller-address-as-id.
    pub origin: String,
    pub amount_raw: String,
    pub reserve_premium_raw: String,
    pub deadline_ms: String,
    pub best_premium_raw: Option<String>,
    pub best_bidder: Option<String>,
    /// open | settled | expired_unsold.
    pub status: String,
    pub winner: Option<String>,
    pub net_premium_raw: Option<String>,
    pub position_id: Option<String>,
    /// Premium before the protocol RFQ fee (settled auctions only).
    pub gross_premium_raw: Option<String>,
    /// Protocol RFQ fee taken at settle (settled auctions only).
    pub fee_raw: Option<String>,
    /// call | put | swap | unknown.
    pub auction_kind: String,
}

impl From<RfqRow> for RfqGql {
    fn from(r: RfqRow) -> Self {
        RfqGql {
            rfq_id: r.rfq_id,
            meta_id: r.meta_id,
            bucket_id: r.bucket_id,
            origin: r.origin,
            amount_raw: r.amount.to_string(),
            reserve_premium_raw: r.reserve_premium.to_string(),
            deadline_ms: r.deadline_ms.to_string(),
            best_premium_raw: r.best_premium.map(|v| v.to_string()),
            best_bidder: r.best_bidder,
            status: r.status,
            winner: r.winner,
            net_premium_raw: r.net_premium.map(|v| v.to_string()),
            position_id: r.position_id,
            gross_premium_raw: r.gross_premium.map(|v| v.to_string()),
            fee_raw: r.fee.map(|v| v.to_string()),
            auction_kind: r.auction_kind,
        }
    }
}

/// One bid in an auction's history (C3).
#[derive(SimpleObject)]
pub struct RfqBidGql {
    pub rfq_id: String,
    pub sequence: String,
    pub bidder: String,
    pub call_recipient: String,
    pub premium_raw: String,
}

impl From<RfqBidRow> for RfqBidGql {
    fn from(r: RfqBidRow) -> Self {
        RfqBidGql {
            rfq_id: r.rfq_id,
            sequence: r.sequence.to_string(),
            bidder: r.bidder,
            call_recipient: r.call_recipient,
            premium_raw: r.premium.to_string(),
        }
    }
}

/// One covered-call vault's headline state (D2).
#[derive(SimpleObject)]
pub struct VaultGql {
    pub vault_id: String,
    pub underlying_type: String,
    pub settlement_type: String,
    pub share_type: String,
    /// Current round (last finalized + 1; 0 = pre-genesis).
    pub round: String,
    pub current_bucket: Option<String>,
    pub latest_pps_raw: Option<String>,
    pub total_shares_raw: String,
    pub pending_deposits_raw: String,
    pub deposits_paused: bool,
    // Active VaultConfig snapshot (consumer-facing subset). Null until the
    // config-carrying events have been indexed for this vault.
    pub mgmt_fee_bps_annual: Option<String>,
    pub perf_fee_bps: Option<String>,
    pub round_ms: Option<String>,
    pub selling_window_ms: Option<String>,
    pub min_strike_bps_over_spot: Option<String>,
    pub max_strike_bps_over_spot: Option<String>,
}

impl From<VaultRow> for VaultGql {
    fn from(v: VaultRow) -> Self {
        VaultGql {
            vault_id: v.vault_id,
            underlying_type: v.underlying_type,
            settlement_type: v.settlement_type,
            share_type: v.share_type,
            round: v.round.to_string(),
            current_bucket: v.current_bucket,
            latest_pps_raw: v.latest_pps.map(|p| p.to_string()),
            total_shares_raw: v.total_shares.to_string(),
            pending_deposits_raw: v.pending_deposits.to_string(),
            deposits_paused: v.deposits_paused,
            mgmt_fee_bps_annual: v.mgmt_fee_bps_annual.map(|x| x.to_string()),
            perf_fee_bps: v.perf_fee_bps.map(|x| x.to_string()),
            round_ms: v.round_ms.map(|x| x.to_string()),
            selling_window_ms: v.selling_window_ms.map(|x| x.to_string()),
            min_strike_bps_over_spot: v.min_strike_bps_over_spot.map(|x| x.to_string()),
            max_strike_bps_over_spot: v.max_strike_bps_over_spot.map(|x| x.to_string()),
        }
    }
}

/// One round in a vault's track record (D2). Selection fields land at
/// `select_bucket`; pps/aum/premium at finalize.
#[derive(SimpleObject)]
pub struct VaultRoundGql {
    pub vault_id: String,
    pub round: String,
    pub bucket_id: Option<String>,
    pub strike_raw: Option<String>,
    pub strike_scale: Option<i32>,
    pub expiry_ms: Option<String>,
    pub pps_raw: Option<String>,
    pub aum_raw: Option<String>,
    pub shares_raw: Option<String>,
    pub premium_collected_raw: Option<String>,
    pub mgmt_fee_raw: Option<String>,
    pub perf_fee_raw: Option<String>,
    pub finalized_at_ms: Option<String>,
}

impl From<VaultRoundRow> for VaultRoundGql {
    fn from(r: VaultRoundRow) -> Self {
        VaultRoundGql {
            vault_id: r.vault_id,
            round: r.round.to_string(),
            bucket_id: r.bucket_id,
            strike_raw: r.strike.map(|v| v.to_string()),
            strike_scale: r.strike_scale.map(|v| v as i32),
            expiry_ms: r.expiry_ms.map(|v| v.to_string()),
            pps_raw: r.pps.map(|v| v.to_string()),
            aum_raw: r.aum.map(|v| v.to_string()),
            shares_raw: r.shares.map(|v| v.to_string()),
            premium_collected_raw: r.premium_collected.map(|v| v.to_string()),
            mgmt_fee_raw: r.mgmt_fee.map(|v| v.to_string()),
            perf_fee_raw: r.perf_fee.map(|v| v.to_string()),
            finalized_at_ms: r.finalized_at_ms.map(|v| v.to_string()),
        }
    }
}

/// One realized-APY point: the annualized pps growth from the previous
/// finalized round to `round`, landing at that round's finalize time. Computed
/// from `vault_rounds` (chain data) — the forward-looking *predicted* APY is a
/// separate series served by the price-charting apy sampler.
#[derive(SimpleObject)]
pub struct VaultApyPointGql {
    pub round: String,
    pub t_ms: String,
    pub apy: f64,
}

/// Annualize pps growth between consecutive finalized rounds. The 1e12 pps
/// scale cancels in the ratio, so raw values are fine.
fn realized_apy_points(rows: &[VaultRoundRow]) -> Vec<VaultApyPointGql> {
    use bigdecimal::ToPrimitive;
    const YEAR_MS: f64 = 365.25 * 86_400_000.0;
    let mut fin: Vec<(i64, f64, i64)> = rows
        .iter()
        .filter_map(|r| Some((r.round, r.pps.as_ref()?.to_f64()?, r.finalized_at_ms?)))
        .collect();
    fin.sort_by_key(|(round, _, _)| *round);
    let mut out = Vec::new();
    for w in fin.windows(2) {
        let (_, p0, t0) = w[0];
        let (round1, p1, t1) = w[1];
        if p0 <= 0.0 || t1 <= t0 {
            continue;
        }
        let periods = YEAR_MS / (t1 - t0) as f64;
        let apy = (p1 / p0).powf(periods) - 1.0;
        if apy.is_finite() {
            out.push(VaultApyPointGql {
                round: round1.to_string(),
                t_ms: t1.to_string(),
                apy,
            });
        }
    }
    out
}

/// One (vault, owner, round, kind) receipt aggregate (D2). `amount` is
/// queued underlying for deposits / escrowed shares for withdrawals;
/// `settled` what's been claimed / completed so far.
#[derive(SimpleObject)]
pub struct VaultReceiptGql {
    pub vault_id: String,
    pub owner: String,
    pub round: String,
    /// deposit | withdraw.
    pub kind: String,
    pub amount_raw: String,
    pub settled_raw: String,
}

impl From<VaultReceiptRow> for VaultReceiptGql {
    fn from(r: VaultReceiptRow) -> Self {
        VaultReceiptGql {
            vault_id: r.vault_id,
            owner: r.owner,
            round: r.round.to_string(),
            kind: r.kind,
            amount_raw: r.amount.to_string(),
            settled_raw: r.settled.to_string(),
        }
    }
}

/// One curated trading vault's headline state (SO-282). On-chain integers
/// are decimal strings (precision-safe).
#[derive(SimpleObject)]
pub struct TradingVaultGql {
    pub vault_id: String,
    /// The vault's unit of account (renamed from deposit_asset in SO-370:
    /// deposits may arrive in any allowlisted asset).
    pub accounting_asset: String,
    pub creator: String,
    /// Current curator wallet (updated on TvCuratorRotated).
    pub curator: String,
    pub curator_cap_id: String,
    /// open | closing | closed.
    pub state: String,
    pub lockup_ms: String,
    pub curator_fee_bps: String,
    pub unwind_grace_ms: String,
    pub deposits_paused: bool,
    pub mm_release_enabled: bool,
    pub total_shares_raw: String,
    pub position_count: String,
    pub pending_withdrawals: String,
    /// Observed accounting-asset-per-share price (1e12-scaled).
    pub latest_pps_e12_raw: Option<String>,
    pub updated_at_ms: String,
    /// External MM account wallet (SO-299); null when none is set.
    pub external_account: Option<String>,
    /// Outstanding external exposure (accounting-asset units).
    pub external_exposure: String,
    /// Latest keeper-posted account equity (EquityPosted).
    pub latest_external_equity: Option<String>,
    pub external_equity_updated_at_ms: Option<String>,
    /// NAV from the latest consumed appraisal (accounting-asset units,
    /// decimal string; SO-304).
    pub latest_nav_raw: Option<String>,
    pub nav_updated_at_ms: Option<String>,
    // ── trading-vault v2 (SO-418) ──
    /// 0 = Untranched, 1 = SeniorJunior. Immutable.
    pub structure_code: i32,
    pub senior_hurdle_bps_annual: String,
    pub target_junior_bps: String,
    pub maintenance_junior_bps: String,
    pub upside_code: i32,
    pub residual_participation_bps: String,
    pub total_return_cap_bps: String,
    pub terms_version: String,
    /// 0x-prefixed hex; null when absent.
    pub spec_hash: Option<String>,
    pub senior_shares_raw: String,
    /// Untranched supply lives here (mirrors capital.move).
    pub junior_shares_raw: String,
    pub senior_claim_raw: String,
    pub senior_principal_basis_raw: String,
    /// Waterfall NAV split from the latest TvCapitalSynced.
    pub senior_nav_raw: Option<String>,
    pub junior_nav_raw: Option<String>,
    /// Observed per-tranche pps (1e12-scaled). Untranched vaults keep
    /// using latestPpsE12Raw.
    pub latest_senior_pps_e12_raw: Option<String>,
    pub latest_junior_pps_e12_raw: Option<String>,
    /// 0=Healthy 1=CoverageBreach 2=Impaired 3=ResetPending.
    pub risk_state: i32,
    pub curator_commitment_breached: bool,
    pub impaired_since_ms: Option<String>,
    pub active_junior_generation: String,
    /// Open junior-reset proposal; all null when there is none.
    pub reset_old_generation: Option<String>,
    pub reset_proposed_at_ms: Option<String>,
    pub reset_executable_at_ms: Option<String>,
    pub reset_recorded_nav_raw: Option<String>,
    pub reset_recorded_senior_claim_raw: Option<String>,
    pub reset_recorded_required_deposit_raw: Option<String>,
    /// Terminal settlement pool (frozen at TvSettlementSnapshot).
    pub settled: bool,
    pub settlement_final_nav_raw: Option<String>,
    pub senior_pool_raw: Option<String>,
    pub senior_supply_raw: Option<String>,
    pub junior_pool_raw: Option<String>,
    pub junior_supply_raw: Option<String>,
    pub settlement_snapshot_at_ms: Option<String>,
    /// Cumulative entitlement drawn from the settlement pools.
    pub settlement_redeemed_raw: String,
    /// Per-lane queue cursors: tail = highest requested global_seq + 1,
    /// head = highest fulfilled/settled global_seq + 1.
    pub senior_lane_head: String,
    pub senior_lane_tail: String,
    pub junior_lane_head: String,
    pub junior_lane_tail: String,
}

impl From<TradingVaultRow> for TradingVaultGql {
    fn from(v: TradingVaultRow) -> Self {
        TradingVaultGql {
            vault_id: v.vault_id,
            accounting_asset: v.accounting_asset,
            creator: v.creator,
            curator: v.curator,
            curator_cap_id: v.curator_cap_id,
            state: v.state,
            lockup_ms: v.lockup_ms.to_string(),
            curator_fee_bps: v.curator_fee_bps.to_string(),
            unwind_grace_ms: v.unwind_grace_ms.to_string(),
            deposits_paused: v.deposits_paused,
            mm_release_enabled: v.mm_release_enabled,
            total_shares_raw: v.total_shares.to_string(),
            position_count: v.position_count.to_string(),
            pending_withdrawals: v.pending_withdrawals.to_string(),
            latest_pps_e12_raw: v.latest_pps_e12.map(|p| p.to_string()),
            updated_at_ms: v.updated_at_ms.to_string(),
            external_account: v.external_account,
            external_exposure: v.external_exposure.to_string(),
            latest_external_equity: v.latest_external_equity.map(|e| e.to_string()),
            external_equity_updated_at_ms: v
                .external_equity_updated_at_ms
                .map(|t| t.to_string()),
            latest_nav_raw: v.latest_nav.map(|n| n.to_string()),
            nav_updated_at_ms: v.nav_updated_at_ms.map(|t| t.to_string()),
            structure_code: v.structure_code as i32,
            senior_hurdle_bps_annual: v.senior_hurdle_bps_annual.to_string(),
            target_junior_bps: v.target_junior_bps.to_string(),
            maintenance_junior_bps: v.maintenance_junior_bps.to_string(),
            upside_code: v.upside_code as i32,
            residual_participation_bps: v.residual_participation_bps.to_string(),
            total_return_cap_bps: v.total_return_cap_bps.to_string(),
            terms_version: v.terms_version.to_string(),
            spec_hash: v.spec_hash,
            senior_shares_raw: v.senior_shares.to_string(),
            junior_shares_raw: v.junior_shares.to_string(),
            senior_claim_raw: v.senior_claim.to_string(),
            senior_principal_basis_raw: v.senior_principal_basis.to_string(),
            senior_nav_raw: v.senior_nav.map(|n| n.to_string()),
            junior_nav_raw: v.junior_nav.map(|n| n.to_string()),
            latest_senior_pps_e12_raw: v.latest_senior_pps_e12.map(|p| p.to_string()),
            latest_junior_pps_e12_raw: v.latest_junior_pps_e12.map(|p| p.to_string()),
            risk_state: v.risk_state as i32,
            curator_commitment_breached: v.curator_commitment_breached,
            impaired_since_ms: v.impaired_since_ms.map(|t| t.to_string()),
            active_junior_generation: v.active_junior_generation.to_string(),
            reset_old_generation: v.reset_old_generation.map(|g| g.to_string()),
            reset_proposed_at_ms: v.reset_proposed_at_ms.map(|t| t.to_string()),
            reset_executable_at_ms: v.reset_executable_at_ms.map(|t| t.to_string()),
            reset_recorded_nav_raw: v.reset_recorded_nav.map(|n| n.to_string()),
            reset_recorded_senior_claim_raw: v
                .reset_recorded_senior_claim
                .map(|n| n.to_string()),
            reset_recorded_required_deposit_raw: v
                .reset_recorded_required_deposit
                .map(|n| n.to_string()),
            settled: v.settled,
            settlement_final_nav_raw: v.settlement_final_nav.map(|n| n.to_string()),
            senior_pool_raw: v.senior_pool.map(|n| n.to_string()),
            senior_supply_raw: v.senior_supply.map(|n| n.to_string()),
            junior_pool_raw: v.junior_pool.map(|n| n.to_string()),
            junior_supply_raw: v.junior_supply.map(|n| n.to_string()),
            settlement_snapshot_at_ms: v.settlement_snapshot_at_ms.map(|t| t.to_string()),
            settlement_redeemed_raw: v.settlement_redeemed.to_string(),
            senior_lane_head: v.senior_lane_head.to_string(),
            senior_lane_tail: v.senior_lane_tail.to_string(),
            junior_lane_head: v.junior_lane_head.to_string(),
            junior_lane_tail: v.junior_lane_tail.to_string(),
        }
    }
}

/// One `VaultPosition` NFT's lifecycle + lineage (SO-418). Ownership is
/// NOT indexed (transfers emit no events); api-service resolves current
/// owners JIT from chain. Rows never delete — consumed/settled/burned
/// stay for history.
#[derive(SimpleObject)]
pub struct VaultPositionGql {
    pub position_id: String,
    pub vault_id: String,
    /// 0=Untranched 1=Senior 2=Junior.
    pub tranche: i32,
    pub capital_generation: String,
    pub shares_raw: String,
    pub cost_basis_raw: String,
    pub locked_until_ms: String,
    /// live | queued | consumed | settled | burned.
    pub status: String,
    /// Split lineage: the parent this position was carved out of.
    pub parent_position_id: Option<String>,
    /// Merge lineage: the kept position this one was folded into.
    pub merged_into: Option<String>,
    /// The withdraw request's global_seq once queued (kept after
    /// consumption as the historical join key).
    pub queued_global_seq: Option<String>,
    pub created_at_ms: String,
    pub updated_at_ms: String,
}

impl From<crate::db::models::VaultPositionRow> for VaultPositionGql {
    fn from(p: crate::db::models::VaultPositionRow) -> Self {
        VaultPositionGql {
            position_id: p.position_id,
            vault_id: p.vault_id,
            tranche: p.tranche as i32,
            capital_generation: p.capital_generation.to_string(),
            shares_raw: p.shares.to_string(),
            cost_basis_raw: p.cost_basis.to_string(),
            locked_until_ms: p.locked_until_ms.to_string(),
            status: p.status,
            parent_position_id: p.parent_position_id,
            merged_into: p.merged_into,
            queued_global_seq: p.queued_global_seq.map(|v| v.to_string()),
            created_at_ms: p.created_at_ms.to_string(),
            updated_at_ms: p.updated_at_ms.to_string(),
        }
    }
}

/// One adapter position held by a trading vault. Removed positions stay
/// with `active=false` so past positions render.
#[derive(SimpleObject)]
pub struct TradingVaultPositionGql {
    pub vault_id: String,
    pub position_id: String,
    pub adapter: String,
    pub active: bool,
    pub stored_at_ms: String,
    pub removed_at_ms: Option<String>,
    /// Latest appraisal mark (accounting-asset units, decimal string;
    /// SO-304). Null until the position is first appraised.
    pub last_value_raw: Option<String>,
    pub last_appraised_at_ms: Option<String>,
}

impl From<TradingVaultPositionRow> for TradingVaultPositionGql {
    fn from(p: TradingVaultPositionRow) -> Self {
        TradingVaultPositionGql {
            vault_id: p.vault_id,
            position_id: p.position_id,
            adapter: p.adapter,
            active: p.active,
            stored_at_ms: p.stored_at_ms.to_string(),
            removed_at_ms: p.removed_at_ms.map(|v| v.to_string()),
            last_value_raw: p.last_value.map(|v| v.to_string()),
            last_appraised_at_ms: p.last_appraised_at_ms.map(|v| v.to_string()),
        }
    }
}

#[derive(SimpleObject)]
pub struct EventConnection {
    pub nodes: Vec<EventGql>,
    /// Pass back as `after` to fetch the next page; null when exhausted.
    pub next_cursor: Option<String>,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum EventOrder {
    SequenceDesc,
    SequenceAsc,
}

/// Recursive event filter. Everything is ANDed at one level; use `and`/`or`/
/// `not` to compose. `payloadContains` (JSONB `@>`) is the general matcher;
/// `participant` matches an address in any role.
#[derive(InputObject, Default)]
pub struct EventFilterInput {
    pub and: Option<Vec<EventFilterInput>>,
    pub or: Option<Vec<EventFilterInput>>,
    pub not: Option<Box<EventFilterInput>>,
    pub event_type: Option<Vec<String>>,
    pub participant: Option<String>,
    pub account_id: Option<String>,
    pub bucket_id: Option<String>,
    pub payload_contains: Option<Json<serde_json::Value>>,
    pub timestamp_ms_gte: Option<i64>,
    pub timestamp_ms_lte: Option<i64>,
    pub sequence_gt: Option<i64>,
    pub sequence_lt: Option<i64>,
    pub checkpoint_gte: Option<i64>,
    pub checkpoint_lte: Option<i64>,
    pub tx_digest: Option<String>,
}

impl EventFilterInput {
    fn into_domain(self) -> EventFilter {
        EventFilter {
            and: self
                .and
                .unwrap_or_default()
                .into_iter()
                .map(Self::into_domain)
                .collect(),
            or: self
                .or
                .unwrap_or_default()
                .into_iter()
                .map(Self::into_domain)
                .collect(),
            not: self.not.map(|b| Box::new(b.into_domain())),
            event_type: self.event_type,
            participant: self.participant,
            account_id: self.account_id,
            bucket_id: self.bucket_id,
            payload_contains: self.payload_contains.map(|j| j.0),
            timestamp_ms_gte: self.timestamp_ms_gte,
            timestamp_ms_lte: self.timestamp_ms_lte,
            sequence_gt: self.sequence_gt,
            sequence_lt: self.sequence_lt,
            checkpoint_gte: self.checkpoint_gte,
            checkpoint_lte: self.checkpoint_lte,
            tx_digest: self.tx_digest,
        }
    }
}

/// Run a blocking Diesel query off the async runtime. `spawn_blocking` breaks
/// span parenting, so capture the resolver's current span (the request span
/// from `http_obs`) and re-enter it inside the closure — the `db_query` child
/// span then lands in the request's Tempo trace. Also records the query
/// duration in `indexer_db_query_duration_seconds{query}`.
async fn db_query<T, F>(query: &'static str, f: F) -> async_graphql::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _e = span.enter();
        let _q = tracing::info_span!("db_query", query).entered();
        let start = std::time::Instant::now();
        let result = f();
        metrics::histogram!("indexer_db_query_duration_seconds", "query" => query)
            .record(start.elapsed().as_secs_f64());
        result
    })
    .await
    .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
    .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Enriched positions for the given on-chain `Position` object ids.
    /// Unknown ids are omitted; the caller renders those degraded.
    async fn positions(
        &self,
        ctx: &Context<'_>,
        object_ids: Vec<String>,
    ) -> async_graphql::Result<Vec<PositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("positions_by_object_ids", move || {
            repo.positions_by_object_ids(&object_ids)
        })
        .await?;
        Ok(rows.into_iter().map(PositionGql::from).collect())
    }

    /// JIT: one bucket by id, or null if unknown.
    async fn bucket(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<BucketGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("bucket_by_id", move || -> anyhow::Result<_> {
            let Some(row) = repo.bucket_by_id(&id)? else {
                return Ok(None);
            };
            let pools = repo.deepbook_pool_ids(std::slice::from_ref(&row.bucket_id))?;
            let pool_id = pools.get(&row.bucket_id).cloned();
            let markets = repo.exchange_market_ids(std::slice::from_ref(&row.bucket_id))?;
            let market_id = markets.get(&row.bucket_id).cloned();
            Ok(Some((row, pool_id, market_id)))
        })
        .await?;
        Ok(row.map(|(b, pool_id, market_id)| BucketGql::from_row(b, pool_id, market_id)))
    }

    /// JIT: buckets matching the given filters (all ANDed). `activeOnly`
    /// drops cleaned buckets.
    async fn buckets(
        &self,
        ctx: &Context<'_>,
        active_only: Option<bool>,
        ids: Option<Vec<String>>,
        asset_type: Option<String>,
        settlement_type: Option<String>,
        expiry_ms: Option<String>,
        option_kind: Option<String>,
    ) -> async_graphql::Result<Vec<BucketGql>> {
        let expiry_ms = match expiry_ms.as_deref() {
            Some(s) => Some(
                s.parse::<i64>()
                    .map_err(|e| async_graphql::Error::new(format!("bad expiryMs {s:?}: {e}")))?,
            ),
            None => None,
        };
        let q = BucketQuery {
            active_only: active_only.unwrap_or(false),
            ids,
            asset_type,
            settlement_type,
            expiry_ms,
            option_kind,
        };
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("buckets_query", move || -> anyhow::Result<_> {
            let rows = repo.buckets_query(q)?;
            let ids: Vec<String> = rows.iter().map(|b| b.bucket_id.clone()).collect();
            let mut pools = repo.deepbook_pool_ids(&ids)?;
            let mut markets = repo.exchange_market_ids(&ids)?;
            Ok(rows
                .into_iter()
                .map(|b| {
                    let pool_id = pools.remove(&b.bucket_id);
                    let market_id = markets.remove(&b.bucket_id);
                    BucketGql::from_row(b, pool_id, market_id)
                })
                .collect::<Vec<_>>())
        })
        .await?;
        Ok(rows)
    }

    /// JIT: one QuoteSigner registration (signing key), or null if unknown.
    async fn account(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<AccountGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("account_by_id", move || repo.account_by_id(&id)).await?;
        Ok(row.map(AccountGql::build))
    }

    /// JIT: enriched positions held by `recipient` (mint-time owner-of-record).
    async fn positions_by_recipient(
        &self,
        ctx: &Context<'_>,
        recipient: String,
    ) -> async_graphql::Result<Vec<PositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("positions_by_recipient", move || {
            repo.positions_by_recipient(&recipient)
        })
        .await?;
        Ok(rows.into_iter().map(PositionGql::from).collect())
    }

    /// JIT: RFQ auctions, optionally filtered by status
    /// (open | settled | expired_unsold) and/or origin (vault id).
    async fn rfqs(
        &self,
        ctx: &Context<'_>,
        status: Option<String>,
        origin: Option<String>,
    ) -> async_graphql::Result<Vec<RfqGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || {
            repo.rfqs_query(status.as_deref(), origin.as_deref())
        })
        .await
        .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
        .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(RfqGql::from).collect())
    }

    /// JIT: bid history for one auction, ascending.
    async fn rfq_bids(
        &self,
        ctx: &Context<'_>,
        rfq_id: String,
    ) -> async_graphql::Result<Vec<RfqBidGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.rfq_bids_for(&rfq_id))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(RfqBidGql::from).collect())
    }

    /// JIT: all covered-call vaults.
    async fn vaults(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<VaultGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.vaults_query())
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(VaultGql::from).collect())
    }

    /// JIT: one vault by id, or null if unknown.
    async fn vault(
        &self,
        ctx: &Context<'_>,
        id: String,
    ) -> async_graphql::Result<Option<VaultGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = tokio::task::spawn_blocking(move || repo.vault_by_id(&id))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(row.map(VaultGql::from))
    }

    /// JIT: one vault's round history, ascending (the track record).
    async fn vault_rounds(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<VaultRoundGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.vault_rounds_for(&vault_id))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(VaultRoundGql::from).collect())
    }

    /// JIT: one vault's realized-APY series (annualized pps growth per
    /// finalized round). Empty until two rounds have finalized.
    async fn vault_apy(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<VaultApyPointGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.vault_rounds_for(&vault_id))
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(realized_apy_points(&rows))
    }

    /// JIT: receipt aggregates for one vault, optionally scoped to an owner.
    async fn vault_receipts(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
        owner: Option<String>,
    ) -> async_graphql::Result<Vec<VaultReceiptGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || {
            repo.vault_receipts_for(&vault_id, owner.as_deref())
        })
        .await
        .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
        .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(VaultReceiptGql::from).collect())
    }

    /// JIT: all curated trading vaults (SO-282).
    async fn trading_vaults(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Vec<TradingVaultGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = tokio::task::spawn_blocking(move || repo.trading_vaults_query())
            .await
            .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
            .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(TradingVaultGql::from).collect())
    }

    /// JIT: adapter positions for one trading vault (removed ones included,
    /// active=false).
    async fn trading_vault_positions(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<TradingVaultPositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows =
            tokio::task::spawn_blocking(move || repo.trading_vault_positions_query(&vault_id))
                .await
                .map_err(|e| async_graphql::Error::new(format!("join error: {e}")))?
                .map_err(|e| async_graphql::Error::new(format!("db error: {e}")))?;
        Ok(rows.into_iter().map(TradingVaultPositionGql::from).collect())
    }

    /// JIT: VaultPosition NFT lifecycle rows for one trading vault
    /// (SO-418), all statuses, ascending by creation time.
    async fn vault_positions(
        &self,
        ctx: &Context<'_>,
        vault_id: String,
    ) -> async_graphql::Result<Vec<VaultPositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let rows = db_query("vault_positions_query", move || {
            repo.vault_positions_query(&vault_id)
        })
        .await?;
        Ok(rows.into_iter().map(VaultPositionGql::from).collect())
    }

    /// JIT: one VaultPosition lifecycle row by its object id (SO-418),
    /// or null if unknown.
    async fn vault_position(
        &self,
        ctx: &Context<'_>,
        position_id: String,
    ) -> async_graphql::Result<Option<VaultPositionGql>> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let row = db_query("vault_position_by_id", move || {
            repo.vault_position_by_id(&position_id)
        })
        .await?;
        Ok(row.map(VaultPositionGql::from))
    }

    /// Generalized event query over the full `indexed_events` log.
    /// `limit` is clamped to 1..=1000; paginate with the returned `nextCursor`.
    async fn events(
        &self,
        ctx: &Context<'_>,
        filter: Option<EventFilterInput>,
        order: Option<EventOrder>,
        limit: Option<i32>,
        after: Option<String>,
    ) -> async_graphql::Result<EventConnection> {
        let repo = ctx.data_unchecked::<Repo>().clone();
        let limit = i64::from(limit.unwrap_or(100).clamp(1, 1000));
        let descending = !matches!(order, Some(EventOrder::SequenceAsc));
        let after_sequence = match after.as_deref() {
            Some(s) => Some(
                s.parse::<i64>()
                    .map_err(|e| async_graphql::Error::new(format!("bad cursor {s:?}: {e}")))?,
            ),
            None => None,
        };
        let q = EventQuery {
            filter: filter.map(EventFilterInput::into_domain),
            descending,
            after_sequence,
            limit,
        };
        let rows = db_query("query_events", move || repo.query_events(q)).await?;
        // Full page ⇒ there may be more; cursor is the last sequence seen.
        let next_cursor = (rows.len() as i64 == limit)
            .then(|| rows.last().map(|r| r.sequence.to_string()))
            .flatten();
        Ok(EventConnection {
            nodes: rows.into_iter().map(EventGql::from).collect(),
            next_cursor,
        })
    }
}

pub type IndexerSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

#[cfg(test)]
mod tests {
    use super::*;

    /// SO-418: pin the exact GraphQL field names the indexer-graphql client
    /// and api-service query — camelCase rendering of e.g. `_e12_raw` and
    /// the new vaultPositions queries must not drift.
    #[test]
    fn schema_exposes_trading_vault_v2_fields() {
        let sdl = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
            .finish()
            .sdl();
        for field in [
            // TradingVaultGql v2 additions.
            "structureCode",
            "seniorHurdleBpsAnnual",
            "targetJuniorBps",
            "maintenanceJuniorBps",
            "upsideCode",
            "residualParticipationBps",
            "totalReturnCapBps",
            "termsVersion",
            "specHash",
            "seniorSharesRaw",
            "juniorSharesRaw",
            "seniorClaimRaw",
            "seniorPrincipalBasisRaw",
            "seniorNavRaw",
            "juniorNavRaw",
            "latestSeniorPpsE12Raw",
            "latestJuniorPpsE12Raw",
            "riskState",
            "curatorCommitmentBreached",
            "impairedSinceMs",
            "activeJuniorGeneration",
            "resetOldGeneration",
            "resetProposedAtMs",
            "resetExecutableAtMs",
            "resetRecordedNavRaw",
            "resetRecordedSeniorClaimRaw",
            "resetRecordedRequiredDepositRaw",
            "settled",
            "settlementFinalNavRaw",
            "seniorPoolRaw",
            "seniorSupplyRaw",
            "juniorPoolRaw",
            "juniorSupplyRaw",
            "settlementSnapshotAtMs",
            "settlementRedeemedRaw",
            "seniorLaneHead",
            "seniorLaneTail",
            "juniorLaneHead",
            "juniorLaneTail",
            // VaultPositionGql + queries.
            "vaultPositions",
            "vaultPosition",
            "positionId",
            "capitalGeneration",
            "costBasisRaw",
            "parentPositionId",
            "mergedInto",
            "queuedGlobalSeq",
        ] {
            assert!(sdl.contains(field), "schema is missing field {field:?}");
        }
    }
}

async fn graphiql() -> impl IntoResponse {
    Html(GraphiQLSource::build().endpoint("/graphql").finish())
}

/// `GET /progress` — checkpoint-ingestion status for the frontend Debug page
/// (SO-107). Plain REST (not GraphQL) so it's a trivial fetch with no query.
async fn progress(Extension(state): Extension<Arc<ProgressState>>) -> axum::Json<ProgressSnapshot> {
    axum::Json(state.snapshot())
}

/// Build the CORS layer from the configured allow-list. `["*"]` (or any entry
/// of `"*"`) permits any origin; otherwise the listed origins are allowed.
/// Mirrors the helper in api-service / token-info so the services behave alike.
fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any))
}

/// Serve the GraphQL API at `POST /graphql` and the `GET /progress` status
/// endpoint on `addr`. CORS is scoped by `allowed_origins`. When
/// `expose_playground` is set, the GraphiQL UI is served on `GET /graphql` and
/// introspection is enabled; otherwise both are disabled (the deployed default
/// for any internet-reachable env).
pub async fn serve(
    addr: SocketAddr,
    repo: Repo,
    progress_state: Arc<ProgressState>,
    allowed_origins: &[String],
    expose_playground: bool,
) -> Result<()> {
    let mut builder = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(repo)
        .limit_depth(15)
        .limit_complexity(1000);
    if !expose_playground {
        builder = builder.disable_introspection();
    }
    let schema = builder.finish();
    let cors = build_cors(allowed_origins)?;
    let graphql_service = GraphQL::new(schema);
    let graphql_route = if expose_playground {
        get(graphiql).post_service(graphql_service)
    } else {
        post_service(graphql_service)
    };
    let app = Router::new()
        .route("/graphql", graphql_route)
        .route("/progress", get(progress))
        .layer(Extension(progress_state))
        .layer(cors)
        .layer(axum::middleware::from_fn(observability::middleware::http_obs));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, expose_playground, "indexer graphql listening");
    axum::serve(listener, app).await?;
    Ok(())
}
