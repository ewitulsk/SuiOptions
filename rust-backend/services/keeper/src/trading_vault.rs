//! Trading-vault keeper pass (SO-287/SO-290): the liveness layer for
//! everything the contracts made permissionless. Per tick, per vault:
//!
//!   1. Settle finished RFQ auctions (tickets whose auction deadline
//!      passed) — winner or not, the escrow/premium/Position come home.
//!   2. Redeem expired option positions (options_adapter and vault_mm
//!      tagged alike).
//!   3. Sweep DeepBook settled amounts into each custody's manager.
//!   4. Sweep vault_mm transfer-ins parked at the vault's address
//!      (Positions, option coins, premium coins).
//!   5. Force-unwind when the OLDEST withdrawal head across BOTH lanes
//!      (v2: senior + junior FIFO lanes under one global sequence) has
//!      aged past the vault's grace period: cancel books + sweep manager
//!      balances. A class-blocked junior head still counts as unmet exit
//!      demand.
//!   6. Post external-account equity into the `EquityBook` (SO-299) when
//!      a venue source has an opinion, stepping within the book's
//!      on-chain guardrails ([`crate::venue_equity`]; the keeper wallet
//!      must be an allowlisted poster). While an external account is still
//!      unfunded, create its zero entry instead (SO-310, permissionless).
//!   7. Fulfill the withdrawal lanes with a FULL attestation-bearing
//!      appraisal (sui_tx::tx::appraisal composer) — cash-only vaults
//!      need no price legs, everything else gets Pyth attestations;
//!      external-configured vaults get the mandatory equity leg. The v2
//!      plan merges both lanes by global sequence, skipping the junior
//!      lane while the vault is risk-off (the contract itself picks the
//!      lowest payable head, so the plan is a conservative superset).
//!      Accounting-payable heads use `fulfill_withdrawals`; heads
//!      requesting a non-accounting payout go through the fulfillment
//!      potato with per-payout-asset attestations (SO-370).
//!   8. When nothing needs fulfilling, run the permissionless
//!      `crank_capital` at `mark_refresh_interval_ms` cadence (SO-418):
//!      the same composed appraisal now also drives hurdle accrual, the
//!      waterfall, risk-state transitions and the commitment test. The
//!      hurdle accrual cap makes this cadence a correctness obligation
//!      (`tv-accrual-cadence`), not just mark freshness.
//!   9. Terminal settlement (v2 §8.7): a Closed vault without a
//!      settlement snapshot gets the permissionless `snapshot_settlement`
//!      crank (`tv-settlement-missing` after 1h); once settled, every
//!      outstanding queued request is paid via `settle_queued_request`;
//!      a settled vault with zero pending requests is skipped forever.
//!
//! Alongside the cranks, a read-only reconciliation monitor
//! (`hedge-reconciliation` alert) compares each external account's
//! recorded exposure against its attested equity every tick, and a
//! capital-state monitor raises `tv-coverage-breach` / `tv-impaired` /
//! `tv-reset-proposed` / `tv-commitment-breach` once per transition.
//!
//! Governance/object ids come from token-info's `trading_vault_objects`
//! block when present (written by the deploy-time activation, SO-292);
//! deployments that predate it fall back to publish-effects discovery.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use indexer_graphql::IndexerClient;
use serde_json::Value;
use sui_tx::chain::{created_objects, ChainClient};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, TransactionKind};
use tracing::{debug, info, warn};

use protocol_types::PriceFeedId;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::appraisal::{
    compose_appraisal, compose_switchboard_appraisal, discover_holdings, price_assets_needed,
    AppraisalRefs, OptionBucketInfo, PositionInfo, VaultHoldings,
};
use sui_tx::tx::pyth_update::PythHandles;
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

use crate::discovery::{price_info_object_for, resolve_price_info_table_from, PriceInfoTable};
use crate::venue_equity::{clamp_step, VenueEquitySource};

use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use std::str::FromStr;

pub struct TradingVaultCtx {
    pub trading_vault_pkg: ObjectID,
    pub oracle_pyth_pkg: ObjectID,
    pub deepbook_adapter_pkg: Option<ObjectID>,
    pub options_adapter_pkg: Option<ObjectID>,
    pub exchange_adapter_pkg: Option<ObjectID>,
    pub core_pkg: ObjectID,
    pub protocol_config_id: ObjectID,
    pub integration_registry_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub pyth_feed_registry_id: ObjectID,
    /// options_core ProtocolConfig + Treasury, for RFQ settles.
    pub core_protocol_config_id: ObjectID,
    pub treasury_id: ObjectID,
    pub gas_budget: u64,
    pub hermes_url: String,
    pub pyth: PythHandles,
    /// canonical coin type → Pyth feed, from the token catalog.
    pub feeds: BTreeMap<String, PriceFeedId>,
    pub price_table: Option<PriceInfoTable>,
    /// equity-oracle package + its shared `EquityBook` (SO-299); `None`
    /// where the package isn't deployed / the book is undiscoverable —
    /// external-configured vaults then skip fulfillment with a clear log.
    pub equity_oracle_pkg: Option<ObjectID>,
    pub equity_book_id: Option<ObjectID>,
    /// Venue equity source feeding the poster crank.
    pub equity_source: Box<dyn VenueEquitySource>,
    /// options-adapter's shared `VolBook` (premium marks, SO-299
    /// follow-up); `None` where undiscoverable — option-coin marks stay
    /// intrinsic-only and the vol crank skips.
    pub vol_book_id: Option<ObjectID>,
    /// oracle-service client + realized-vol window for the vol crank.
    pub oracle: oracle_client::OracleClient,
    /// TTL-cached `/oracle/descriptor` (SO-346): the appraisal composer
    /// re-reads the live provider at runtime, so a provider flip is an
    /// oracle-service restart and NO keeper restart.
    pub descriptor_cache:
        std::sync::Mutex<Option<(std::time::Instant, oracle_client::OracleDescriptor)>>,
    pub vol_window_days: u32,
    /// `hedge-reconciliation` thresholds (keeper config `[external]`).
    pub reconciliation_tolerance_bps: u64,
    pub equity_stale_alert_ms: u64,
    /// Per-vault last mark-refresh time (crank 8, SO-304).
    pub mark_refreshed_at: std::sync::Mutex<BTreeMap<ObjectID, u64>>,
    /// Minimum spacing between per-vault mark-refresh cranks (SO-304):
    /// the tick loop runs much faster than fresh marks are worth their
    /// gas. Config `mark_refresh_interval_ms` — the main gas knob.
    pub mark_refresh_interval_ms: u64,
    /// Per-vault crank backoff (SO-346): consecutive non-benign failure
    /// count + earliest next attempt. Without it a persistently failing
    /// vault retries at FULL TICK RATE, and one bad night multiplied a
    /// per-vault failure into hundreds of paid reverts.
    pub crank_backoff: std::sync::Mutex<BTreeMap<ObjectID, (u32, u64)>>,
    /// Per-vault last-seen capital state (SO-418): risk-state alerts fire
    /// once per TRANSITION, not every tick.
    pub capital_watch: std::sync::Mutex<BTreeMap<ObjectID, CapitalWatch>>,
    /// Closed-but-unsettled vaults: (first seen at, alerted). Drives the
    /// `tv-settlement-missing` 1h incident clock.
    pub settlement_watch: std::sync::Mutex<BTreeMap<ObjectID, (u64, bool)>>,
    /// Vaults confirmed Closed && settled && zero pending requests —
    /// terminal forever (requests cannot be added once Closed), skipped
    /// without a chain read.
    pub settled_done: std::sync::Mutex<std::collections::BTreeSet<ObjectID>>,
}

/// Mirror of `capital::ACCRUAL_CAP_MS` (2 years): the overflow sanity
/// bound on hurdle accrual. Crank cadence must stay ≪ this or a vault
/// silently under-accrues — config validation rejects
/// `mark_refresh_interval_ms ≥ cap/1000` and `tv-accrual-cadence` fires
/// when a tranched vault's last capital sync ages past cap/100.
pub const ACCRUAL_CAP_MS: u64 = 63_072_000_000;

/// Last-seen capital state per vault, for transition-edge alerting.
#[derive(Default, Clone, Copy)]
pub struct CapitalWatch {
    pub risk_state: u8,
    pub reset_proposed: bool,
    pub commitment_breached: bool,
    pub accrual_alerted: bool,
}

/// Backoff schedule for non-benign crank failures: tick-rate on the
/// first failure, doubling to a 10-minute cap. Benign races (settle
/// lost, appraisal raced) never back off — they resolve next tick.
const CRANK_BACKOFF_BASE_MS: u64 = 15_000;
const CRANK_BACKOFF_CAP_MS: u64 = 600_000;

/// Indexer view of a vault's external account, threaded into the tick.
pub struct ExternalView {
    pub account: SuiAddress,
    pub exposure: u64,
    pub equity: Option<u64>,
    pub equity_updated_at_ms: Option<u64>,
}

/// The governance-object bundle, resolved from token-info or (fallback)
/// from the packages' publish effects. Mirrors the deploy-time record.
pub struct GovernanceObjects {
    pub protocol_config_id: ObjectID,
    pub integration_registry_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub pyth_feed_registry_id: ObjectID,
}

async fn created_of_types(
    client: &ChainClient,
    publish_digest: &str,
    wanted: &[&str],
) -> Result<BTreeMap<String, ObjectID>> {
    let digest = publish_digest.parse().context("parsing publish digest")?;
    let resp = client
        .get_transaction(&digest)
        .await
        .context("fetching publish tx")?;
    let mut out = BTreeMap::new();
    // The changed-objects list carries the type inline; only when a pruned
    // node omits it do we fall back to reading each created object.
    let created = created_objects(&resp);
    for change in &created {
        if let Ok(tag) = sui_types::parse_sui_struct_tag(&change.object_type) {
            let key = format!("{}::{}", tag.module, tag.name);
            if wanted.iter().any(|w| key == *w) {
                out.insert(key, change.object_id);
            }
        }
    }
    if out.len() < wanted.len() {
        let ids: Vec<ObjectID> = created.iter().map(|c| c.object_id).collect();
        let objs = client
            .multi_get_objects(&ids)
            .await
            .context("resolving created objects")?;
        for o in objs {
            if let Some(t) = o.struct_tag() {
                let full = t.to_canonical_string(/* with_prefix */ true);
                for w in wanted {
                    if full.ends_with(&format!("::{w}")) {
                        out.insert((*w).to_string(), o.id());
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Fallback discovery for deployments whose token-info predates the
/// `trading_vault_objects` block.
pub async fn discover_governance(
    client: &ChainClient,
    trading_vault_digest: &str,
    oracle_pyth_digest: &str,
) -> Result<GovernanceObjects> {
    let tv = created_of_types(
        client,
        trading_vault_digest,
        &[
            "registry::VaultProtocolConfig",
            "registry::IntegrationRegistry",
            "registry::OracleRegistry",
        ],
    )
    .await?;
    let op = created_of_types(client, oracle_pyth_digest, &["oracle_pyth::PythFeedRegistry"])
        .await?;
    let pick = |m: &BTreeMap<String, ObjectID>, k: &str| {
        m.get(k).copied().ok_or_else(|| anyhow!("{k} not found in publish effects"))
    };
    Ok(GovernanceObjects {
        protocol_config_id: pick(&tv, "registry::VaultProtocolConfig")?,
        integration_registry_id: pick(&tv, "registry::IntegrationRegistry")?,
        oracle_registry_id: pick(&tv, "registry::OracleRegistry")?,
        pyth_feed_registry_id: pick(&op, "oracle_pyth::PythFeedRegistry")?,
    })
}

/// One tick over every trading vault. Failures are contained per vault
/// per crank; only genuinely unexpected ones raise the alert.
pub async fn tick(wrap: &SuiClientWrapper, http: &reqwest::Client, indexer: &IndexerClient, ctx: &TradingVaultCtx) {
    let vaults = match indexer.trading_vaults().await {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %format!("{e:#}"), "trading-vault discovery failed; next tick");
            return;
        }
    };
    // Option-coin type → bucket map for appraisal legs. Expired buckets
    // included — their coins still need (zero/dust) marks. Failure is
    // non-fatal: cash-only vaults never consult the map.
    let option_buckets = match indexer.buckets(false, None, None, None).await {
        Ok(bs) => bs
            .into_iter()
            .map(|b| {
                (
                    protocol_types::asset::canonicalize_move_type(b.call_type.as_str()),
                    OptionBucketInfo {
                        bucket_id: ObjectID::from_hex_literal(&b.bucket_id.to_hex())
                            .unwrap_or(ObjectID::ZERO),
                        underlying: protocol_types::asset::canonicalize_move_type(
                            b.asset_type.as_str(),
                        ),
                        settlement: protocol_types::asset::canonicalize_move_type(
                            b.settlement_type.as_str(),
                        ),
                        is_put: b.option_kind == "put",
                    },
                )
            })
            .filter(|(_, b)| b.bucket_id != ObjectID::ZERO)
            .collect(),
        Err(e) => {
            debug!(error = %format!("{e:#}"), "bucket map fetch failed; option-coin legs unavailable this tick");
            std::collections::BTreeMap::new()
        }
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // EquityBook guardrail params (min_interval_ms, max_delta_bps), read
    // once per tick — only when an external-configured vault exists.
    let book_params = match ctx.equity_book_id {
        Some(book_id) if vaults.iter().any(|v| v.external_account.is_some()) => {
            match equity_book_params(&wrap.client, book_id).await {
                Ok(p) => Some(p),
                Err(e) => {
                    debug!(error = %format!("{e:#}"), "EquityBook params unreadable; no equity posts this tick");
                    None
                }
            }
        }
        _ => None,
    };
    post_vols(wrap, ctx, now_ms).await;
    for v in vaults {
        let vault_id = match ObjectID::from_hex_literal(&v.vault_id.to_hex()) {
            Ok(id) => id,
            Err(_) => continue,
        };
        // v2: "fully closed" means SETTLED — a Closed vault still needs
        // its one-shot settlement snapshot and its queued requests paid
        // from the pool. Only a vault confirmed settled with zero pending
        // requests is terminal (cached: that state can never regress).
        if ctx.settled_done.lock().expect("settled_done poisoned").contains(&vault_id) {
            continue;
        }
        let external = v.external_account.as_ref().and_then(|a| {
            Some(ExternalView {
                account: SuiAddress::from_bytes(a.as_bytes()).ok()?,
                exposure: v.external_exposure,
                equity: v.latest_external_equity,
                equity_updated_at_ms: v.external_equity_updated_at_ms,
            })
        });
        if let Some(ext) = &external {
            monitor_external(ctx, vault_id, ext, now_ms);
        }
        // Per-vault backoff (SO-346): a vault whose cranks keep failing
        // for a real (non-benign) reason waits out its window instead of
        // retrying — and paying — at full tick rate.
        {
            let backoff = ctx.crank_backoff.lock().expect("crank_backoff poisoned");
            if let Some((fails, until)) = backoff.get(&vault_id) {
                if now_ms < *until {
                    debug!(vault = %vault_id, fails, wait_ms = *until - now_ms, "crank backoff; skipping this tick");
                    continue;
                }
            }
        }
        match tick_one(
            wrap,
            http,
            ctx,
            vault_id,
            &option_buckets,
            external.as_ref(),
            book_params,
        )
        .await
        {
            Ok(()) => {
                ctx.crank_backoff.lock().expect("crank_backoff poisoned").remove(&vault_id);
            }
            Err(e) => {
                let benign = classify_and_log(vault_id, &e);
                if !benign {
                    let mut backoff =
                        ctx.crank_backoff.lock().expect("crank_backoff poisoned");
                    let fails = backoff.get(&vault_id).map(|(f, _)| f + 1).unwrap_or(1);
                    let delay = CRANK_BACKOFF_BASE_MS
                        .saturating_mul(1u64 << fails.min(16))
                        .min(CRANK_BACKOFF_CAP_MS);
                    backoff.insert(vault_id, (fails, now_ms + delay));
                }
            }
        }
    }
}

/// Log a crank failure at the right severity; returns whether it was a
/// benign race (which must NOT feed the backoff — races resolve on the
/// very next tick).
fn classify_and_log(vault_id: ObjectID, e: &anyhow::Error) -> bool {
    use protocol_types::vault_abort;
    let msg = format!("{e:#}");
    let abort = |code: u64| format!(", {code})");
    // Known benign shapes (codes from `protocol_types::vault_abort`, the
    // shared v2 error table): appraisal raced a session
    // (APPRAISAL_MISMATCH), incomplete because holdings changed under us
    // (APPRAISAL_INCOMPLETE), insufficient free balance
    // (INSUFFICIENT_BALANCE), auction not yet past deadline / bucket
    // state races.
    let benign_codes = [
        vault_abort::APPRAISAL_INCOMPLETE,
        vault_abort::APPRAISAL_MISMATCH,
        vault_abort::INSUFFICIENT_BALANCE,
    ];
    let benign_code = benign_codes.iter().any(|c| msg.contains(&abort(*c)));
    let benign_text = ["deadline", "not expired"].iter().any(|b| msg.contains(b));
    // v2 permissionless-crank races: the vault flipped risk-off between
    // compose and execute (RISK_OFF), or a racing cranker settled the
    // queue / snapshot first (QUEUE_SETTLED). Both scoped to vault aborts
    // so an unrelated 124/136 still alerts.
    let risk_race = msg.contains("vault")
        && (msg.contains(&abort(vault_abort::RISK_OFF))
            || msg.contains(&abort(vault_abort::QUEUE_SETTLED)));
    // Equity-oracle E_TOO_SOON (3): another poster (or our previous tick)
    // raced the min-interval window — benign, but ONLY for equity_oracle
    // aborts so an unrelated code-3 abort still alerts. E_STALE (5) and
    // E_NOT_POSTER (1) stay alerting.
    let equity_race = msg.contains("equity_oracle") && msg.contains(", 3)");
    // vol_book E_TOO_SOON (3): same min-interval race shape.
    let vol_race = msg.contains("vol_book") && msg.contains(", 3)");
    // Bid-ticket cranks (SO-299) losing races: still the best bidder when
    // a donated look-alike coin was fed in (options_adapter
    // E_STILL_BEST_BIDDER, 10), ticket already burned by a racing cranker
    // (vault POSITION_MISSING), or the auction/receiving input was
    // consumed between compose and execute.
    let bid_ticket_race = (msg.contains("options_adapter") && msg.contains(", 10)"))
        || (msg.contains("vault") && msg.contains(&abort(vault_abort::POSITION_MISSING)))
        || msg.contains("not available for consumption");
    if benign_code || benign_text || risk_race || equity_race || vol_race || bid_ticket_race {
        debug!(vault = %vault_id, error = %msg, "trading-vault crank lost a race; next tick");
        true
    } else {
        tracing::error!(
            alert_id = "tx-failed-keeper",
            vault = %vault_id,
            class = "retry",
            error = %msg,
            "trading-vault crank failed; backing off"
        );
        false
    }
}

#[allow(clippy::too_many_arguments)]
async fn tick_one(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    external: Option<&ExternalView>,
    book_params: Option<(u64, u64)>,
) -> Result<()> {
    let client = &wrap.client;
    let holdings = discover_holdings(client, vault_id).await?;
    let view = vault_view(client, vault_id).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Capital-state monitor: transition-edge alerts + the accrual
    // cadence SLO, straight off this tick's vault read.
    monitor_capital(ctx, vault_id, &view, now_ms);

    if view.closed {
        // Terminal path (v2 §8.7): drive the one-shot settlement
        // snapshot, then pay outstanding queued requests from the pool.
        // A Closed vault has zero positions and only the accounting
        // asset, so none of the trading cranks below apply.
        return terminal_settlement(wrap, http, ctx, vault_id, &holdings, option_buckets, &view, now_ms)
            .await;
    }
    ctx.settlement_watch.lock().expect("settlement_watch poisoned").remove(&vault_id);

    settle_due_tickets(wrap, ctx, vault_id, &holdings, now_ms).await;
    crank_bid_tickets(wrap, ctx, vault_id, &holdings).await;
    redeem_expired_positions(wrap, ctx, vault_id, &holdings, now_ms).await;
    sweep_custody_settled(wrap, ctx, vault_id, &holdings).await;
    sweep_vault_address(wrap, ctx, vault_id).await;
    // The merged (both-lane, global-sequence-ordered) pending run: drives
    // both the force-unwind age trigger (junior heads count even while
    // class-blocked) and the fulfillment plan.
    let run = queue_run(client, &view, MAX_MIXED_RUN).await?;
    force_unwind_if_starved(wrap, ctx, vault_id, &holdings, &view, &run, now_ms).await;
    // BEFORE the post so a freshly registered account has its zero anchor to
    // step off of.
    init_external_entry(wrap, ctx, vault_id, &holdings).await;
    if let Some(ext) = external {
        // BEFORE fulfillment so its equity leg reads a fresh mark.
        post_external_equity(wrap, ctx, vault_id, ext, book_params, now_ms).await;
    }

    // A run whose every entry is a class-blocked junior head has nothing
    // fulfillable — and MUST fall through to the capital crank below:
    // risk states only cure on consumed appraisals, and fulfillment (the
    // other appraisal consumer) would never submit here.
    let junior_blocked = view.risk_state != 0;
    let has_payable = run.iter().any(|e| !(junior_blocked && e.lane == 1));
    if has_payable {
        // Re-discover: the cranks above may have changed holdings.
        let holdings = discover_holdings(client, vault_id).await?;
        fulfill(wrap, http, ctx, vault_id, &holdings, option_buckets, &view, &run, now_ms).await?;
    } else if (view.tranched || !holdings.is_cash_only()) && mark_refresh_due(ctx, vault_id, now_ms)
    {
        // Crank 8 (SO-304 → SO-418): nothing to fulfill — run the
        // permissionless capital crank. Beyond mark freshness this is a
        // CORRECTNESS cadence on tranched vaults (hurdle accrual,
        // risk-state transitions, commitment test), so those run even
        // when cash-only; untranched cash-only vaults have nothing to
        // mark or accrue and are skipped. Re-discover: the cranks above
        // may have changed holdings.
        let holdings = discover_holdings(client, vault_id).await?;
        if view.tranched || !holdings.is_cash_only() {
            crank_capital_pass(wrap, http, ctx, vault_id, &holdings, option_buckets).await?;
            ctx.mark_refreshed_at
                .lock()
                .expect("mark_refreshed_at poisoned")
                .insert(vault_id, now_ms);
        }
    }
    Ok(())
}

fn mark_refresh_due(ctx: &TradingVaultCtx, vault_id: ObjectID, now_ms: u64) -> bool {
    let last = ctx
        .mark_refreshed_at
        .lock()
        .expect("mark_refreshed_at poisoned")
        .get(&vault_id)
        .copied()
        .unwrap_or(0);
    now_ms.saturating_sub(last) >= ctx.mark_refresh_interval_ms
}

fn refs_for(ctx: &TradingVaultCtx, vault_id: ObjectID) -> AppraisalRefs {
    AppraisalRefs {
        trading_vault_pkg: ctx.trading_vault_pkg,
        deepbook_adapter_pkg: ctx.deepbook_adapter_pkg,
        options_adapter_pkg: ctx.options_adapter_pkg,
        exchange_adapter_pkg: ctx.exchange_adapter_pkg,
        vault_id,
        protocol_config_id: ctx.protocol_config_id,
        oracle_registry_id: ctx.oracle_registry_id,
        equity_oracle_pkg: ctx.equity_oracle_pkg,
        equity_book_id: ctx.equity_book_id,
        vol_book_id: ctx.vol_book_id,
    }
}

/// One withdrawal lane's bounds + its `entries` table (lane idx → global
/// sequence).
struct LaneView {
    head: u64,
    tail: u64,
    entries_table: ObjectID,
}

/// The v2 vault fields the keeper reads every tick, from ONE object
/// fetch. Written against the gRPC JSON rendering: no `fields` wrapper,
/// enums as `{"@variant": …}`, `TypeName` as a bare string
/// (docs/sui-json-rpc-migration.md; api-service goldens are the
/// reference).
struct VaultView {
    /// Lifecycle `Closed` (settlement pool replaces the queue).
    closed: bool,
    /// The one-time settlement snapshot has been taken.
    settled: bool,
    /// `CapitalStructure::SeniorJunior` (untranched vaults are always
    /// Healthy and accrue no hurdle).
    tranched: bool,
    /// Wire code: 0 Healthy, 1 CoverageBreach, 2 Impaired, 3 ResetPending.
    risk_state: u8,
    reset_proposed: bool,
    commitment_breached: bool,
    /// `book.last_accrual_ms` — the last consumed capital sync.
    last_accrual_ms: u64,
    active_junior_generation: u64,
    unwind_grace_ms: u64,
    /// `requests` table (global seq → WithdrawRequest).
    requests_table: ObjectID,
    senior: LaneView,
    junior: LaneView,
}

fn table_id(v: &Value, ptr: &str) -> Result<ObjectID> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .and_then(|s| ObjectID::from_hex_literal(s).ok())
        .ok_or_else(|| anyhow!("vault table id at {ptr} unreadable"))
}

async fn vault_view(client: &ChainClient, vault_id: ObjectID) -> Result<VaultView> {
    let (_, json) = client.get_object_json(vault_id).await?;
    let v = json.ok_or_else(|| anyhow!("vault {vault_id} has no parsed content"))?;
    let variant = |ptr: &str| -> Option<&str> {
        v.pointer(ptr).and_then(|x| x.get("@variant")).and_then(Value::as_str)
    };
    let closed = variant("/state") == Some("Closed");
    // Options render inline: null/absent = none.
    let settled = v.pointer("/settlement").map(|x| !x.is_null()).unwrap_or(false);
    let reset_proposed =
        v.pointer("/book/reset_proposal").map(|x| !x.is_null()).unwrap_or(false);
    let tranched = variant("/capital") == Some("SeniorJunior");
    let risk_state = match variant("/book/risk_state")
        .ok_or_else(|| anyhow!("vault {vault_id} book.risk_state unreadable"))?
    {
        "Healthy" => 0,
        "CoverageBreach" => 1,
        "Impaired" => 2,
        "ResetPending" => 3,
        other => return Err(anyhow!("unknown risk state {other:?}")),
    };
    let commitment_breached = v
        .pointer("/curator_commitment_breached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let num = |ptr: &str| -> Result<u64> {
        as_u64(v.pointer(ptr).ok_or_else(|| anyhow!("vault missing {ptr}"))?)
            .ok_or_else(|| anyhow!("vault {ptr} unreadable"))
    };
    let lane = |name: &str| -> Result<LaneView> {
        Ok(LaneView {
            head: num(&format!("/{name}/head"))?,
            tail: num(&format!("/{name}/tail"))?,
            entries_table: table_id(&v, &format!("/{name}/entries/id"))?,
        })
    };
    Ok(VaultView {
        closed,
        settled,
        tranched,
        risk_state,
        reset_proposed,
        commitment_breached,
        last_accrual_ms: num("/book/last_accrual_ms")?,
        active_junior_generation: num("/book/active_junior_generation")?,
        unwind_grace_ms: num("/config/unwind_grace_ms")?,
        requests_table: table_id(&v, "/requests/id")?,
        senior: lane("senior_lane")?,
        junior: lane("junior_lane")?,
    })
}

/// Capital-state monitor (SO-418, runbooks.md): risk-state transition
/// alerts (once per transition — the last-seen state lives in
/// `ctx.capital_watch`) + the accrual-cadence SLO. Repo convention:
/// alertable conditions log `error!` with an `alert_id` regardless of
/// nominal severity.
fn monitor_capital(ctx: &TradingVaultCtx, vault_id: ObjectID, view: &VaultView, now_ms: u64) {
    let mut watch = ctx.capital_watch.lock().expect("capital_watch poisoned");
    let prev = watch.get(&vault_id).copied().unwrap_or_default();

    if view.risk_state != prev.risk_state {
        match view.risk_state {
            1 => tracing::error!(
                alert_id = "tv-coverage-breach",
                vault = %vault_id,
                "junior buffer below maintenance: junior lane paused, deployment gated"
            ),
            2 => tracing::error!(
                alert_id = "tv-impaired",
                vault = %vault_id,
                "vault impaired: NAV < senior claim — verify marks FIRST, then watch for a reset proposal"
            ),
            // 3 (ResetPending) is alerted through the proposal edge below.
            3 => {}
            _ => info!(
                vault = %vault_id,
                from = prev.risk_state,
                "vault capital state recovered to Healthy"
            ),
        }
    }
    if view.reset_proposed && !prev.reset_proposed {
        tracing::error!(
            alert_id = "tv-reset-proposed",
            vault = %vault_id,
            "junior generational reset proposed — user-facing: surface terms + executable time; \
             recovery before execution cancels it"
        );
    }
    if view.commitment_breached && !prev.commitment_breached {
        tracing::error!(
            alert_id = "tv-commitment-breach",
            vault = %vault_id,
            "curator commitment marked below the protocol floor: deployment halts until \
             deposit_into_commitment cures it"
        );
    }

    // Accrual cadence SLO: consumed capital syncs must stay ≪ the 2y
    // accrual cap or hurdle silently under-accrues (spec §2). Only a
    // correctness bound on tranched vaults.
    let accrual_late =
        view.tranched && now_ms.saturating_sub(view.last_accrual_ms) > ACCRUAL_CAP_MS / 100;
    if accrual_late && !prev.accrual_alerted {
        tracing::error!(
            alert_id = "tv-accrual-cadence",
            vault = %vault_id,
            last_accrual_ms = view.last_accrual_ms,
            age_ms = now_ms.saturating_sub(view.last_accrual_ms),
            "last consumed capital sync is older than 1/100 of the accrual cap — the crank \
             cadence is a correctness obligation"
        );
    }

    watch.insert(
        vault_id,
        CapitalWatch {
            risk_state: view.risk_state,
            reset_proposed: view.reset_proposed,
            commitment_breached: view.commitment_breached,
            accrual_alerted: accrual_late,
        },
    );
}

/// Crank 7: step VolBook entries toward oracle-service realized vol,
/// within the on-chain guardrails (mirrors the equity crank). Only
/// admin-seeded underlyings move; the keeper wallet must be an
/// allowlisted poster (`vol_book::add_poster`). Runs once per tick over
/// the token catalog.
async fn post_vols(wrap: &SuiClientWrapper, ctx: &TradingVaultCtx, now_ms: u64) {
    let Some(book_id) = ctx.vol_book_id else { return };
    if ctx.feeds.is_empty() {
        return;
    }
    let client = &wrap.client;
    let (min_interval_ms, max_delta_bps, entries_table) =
        match vol_book_meta(client, book_id).await {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %format!("{e:#}"), "VolBook unreadable; no vol posts this tick");
                return;
            }
        };
    for (coin_type, feed) in &ctx.feeds {
        let result: Result<()> = async {
            // Only seeded underlyings are postable; a zero entry cannot
            // be moved by a poster (admin re-seed required).
            let Some((previous, updated_at)) =
                vol_entry(client, entries_table, coin_type).await?
            else {
                return Ok(());
            };
            if previous == 0 {
                debug!(underlying = %coin_type, "vol entry is zero; admin seed_vol required");
                return Ok(());
            }
            if now_ms.saturating_sub(updated_at) < min_interval_ms {
                return Ok(());
            }
            let sigma = match ctx.oracle.realized_vol(*feed, ctx.vol_window_days).await {
                Ok(s) => s,
                Err(e) => {
                    debug!(underlying = %coin_type, error = %format!("{e:#}"), "realized vol unavailable; skipping post");
                    return Ok(());
                }
            };
            if !(sigma.is_finite() && sigma > 0.0) {
                return Ok(());
            }
            let target = (sigma * 10_000.0).round() as u64;
            let clamped = clamp_step(previous, target, max_delta_bps);
            if clamped == previous {
                return Ok(());
            }
            let mut pt = ProgrammableTransactionBuilder::new();
            let book = pt.obj(shared_object_arg(client, book_id, true).await?)?;
            let tag = TypeTag::from_str(coin_type)
                .with_context(|| format!("parsing underlying type {coin_type}"))?;
            let underlying = pt.programmable_move_call(
                ObjectID::from_hex_literal("0x1").unwrap(),
                Identifier::new("type_name").unwrap(),
                Identifier::new("with_defining_ids").unwrap(),
                vec![tag],
                vec![],
            );
            let amount = pt.pure(clamped)?;
            let clock = clock_arg(&mut pt)?;
            pt.programmable_move_call(
                ctx.options_adapter_pkg
                    .ok_or_else(|| anyhow!("options_adapter package unresolved"))?,
                Identifier::new("vol_book").unwrap(),
                Identifier::new("post_vol").unwrap(),
                vec![],
                vec![book, underlying, amount, clock],
            );
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "vol_book::post_vol").await?;
            info!(underlying = %coin_type, previous, posted = clamped, target, "realized vol posted");
            Ok(())
        }
        .await;
        if let Err(e) = result {
            classify_and_log(book_id, &e);
        }
    }
}

/// The VolBook's poster guardrails + entries table id:
/// (min_interval_ms, max_delta_bps, entries table).
async fn vol_book_meta(client: &ChainClient, book_id: ObjectID) -> Result<(u64, u64, ObjectID)> {
    let min_interval = as_u64(&json_field(client, book_id, "/min_interval_ms").await?)
        .ok_or_else(|| anyhow!("VolBook min_interval_ms unreadable"))?;
    let max_delta = as_u64(&json_field(client, book_id, "/max_delta_bps").await?)
        .ok_or_else(|| anyhow!("VolBook max_delta_bps unreadable"))?;
    let table = json_field(client, book_id, "/entries/id").await?;
    let table_id = table
        .as_str()
        .and_then(|s| ObjectID::from_hex_literal(s).ok())
        .ok_or_else(|| anyhow!("VolBook entries table id unreadable"))?;
    Ok((min_interval, max_delta, table_id))
}

/// One VolBook entry `(vol_bps, updated_at_ms)`, or `None` when the
/// underlying was never seeded. The table is keyed by
/// `0x1::type_name::TypeName`, whose `name` is the canonical type
/// WITHOUT the `0x` prefix.
async fn vol_entry(
    client: &ChainClient,
    entries_table: ObjectID,
    canonical_type: &str,
) -> Result<Option<(u64, u64)>> {
    // Derived-field read (no dynamic-field index API — publicnode
    // doesn't serve it). `TypeName` BCS == its `name` string.
    let name = canonical_type.trim_start_matches("0x");
    let key_bytes = bcs::to_bytes(&name.to_string()).context("bcs of TypeName")?;
    let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
        entries_table,
        &TypeTag::from_str("0x1::type_name::TypeName").expect("static type tag"),
        &key_bytes,
    )
    .context("deriving VolBook entry field id")?;
    let Some((_, json)) = client
        .try_get_object_json(field_id)
        .await
        .context("reading VolBook entry")?
    else {
        return Ok(None);
    };
    let json = json.ok_or_else(|| anyhow!("VolBook entry has no readable content"))?;
    let pick = |ptr: &str| -> Result<u64> {
        as_u64(
            json.pointer(ptr)
                .ok_or_else(|| anyhow!("VolBook entry missing {ptr}"))?,
        )
        .ok_or_else(|| anyhow!("VolBook entry {ptr} unreadable"))
    };
    Ok(Some((pick("/value/vol_bps")?, pick("/value/updated_at_ms")?)))
}

/// One field off an object's JSON rendering. `pointer` is written against
/// the gRPC/GraphQL rendering, which nests struct fields directly (no
/// `fields` wrapper — see docs/sui-json-rpc-migration.md).
async fn json_field(client: &ChainClient, id: ObjectID, pointer: &str) -> Result<Value> {
    let (_, json) = client.get_object_json(id).await?;
    json.ok_or_else(|| anyhow!("object {id} missing content"))?
        .pointer(pointer)
        .cloned()
        .ok_or_else(|| anyhow!("object {id} missing {pointer}"))
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64())
}

/// Crank 1: settle every ticket whose auction deadline has passed (or
/// whose bucket died).
async fn settle_due_tickets(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    now_ms: u64,
) {
    let Some(oa) = ctx.options_adapter_pkg else { return };
    for p in &holdings.positions {
        let PositionInfo::RfqTicket { id, auction_id, bucket_id, is_put, .. } = p else {
            continue;
        };
        let result: Result<()> = async {
            let client = &wrap.client;
            let deadline = as_u64(
                &json_field(client, *auction_id, "/deadline_ms").await?,
            )
            .ok_or_else(|| anyhow!("auction missing deadline"))?;
            let bucket_ty = object_type_of(client, *bucket_id).await?;
            let expiry = as_u64(&json_field(client, *bucket_id, "/expiry_ms").await?)
                .unwrap_or(u64::MAX);
            let invalidated = json_field(client, *bucket_id, "/invalidated")
                .await
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let bucket_dead = now_ms >= expiry || invalidated;
            if now_ms < deadline && !bucket_dead {
                return Ok(());
            }
            let tags = bucket_type_args(&bucket_ty)?;
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
            let ticket_arg = pt.pure(id)?;
            let auction = pt.obj(shared_object_arg(client, *auction_id, true).await?)?;
            let clock = clock_arg(&mut pt)?;
            let function = match (is_put, bucket_dead) {
                (false, false) => "settle_call_rfq",
                (false, true) => "settle_call_rfq_expired",
                (true, false) => "settle_put_rfq",
                (true, true) => "settle_put_rfq_expired",
            };
            let mut args = vec![vault, ireg, ticket_arg, auction];
            if !bucket_dead {
                let bucket = pt.obj(shared_object_arg(client, *bucket_id, true).await?)?;
                let cfg = pt.obj(shared_object_arg(client, ctx.core_protocol_config_id, false).await?)?;
                let treasury = pt.obj(shared_object_arg(client, ctx.treasury_id, true).await?)?;
                args.extend([bucket, cfg, treasury, clock]);
            } else {
                let bucket = pt.obj(shared_object_arg(client, *bucket_id, false).await?)?;
                args.extend([bucket, clock]);
            }
            pt.programmable_move_call(
                oa,
                Identifier::new("options_adapter").unwrap(),
                Identifier::new(function).unwrap(),
                tags,
                args,
            );
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "options_adapter::settle_rfq")
                .await?;
            info!(vault = %vault_id, ticket = %id, function, "rfq ticket settled");
            Ok(())
        }
        .await;
        if let Err(e) = result {
            classify_and_log(vault_id, &e);
        }
    }
}

/// Crank 1b (SO-299): burn vault-funded `BidTicket`s whose auction has
/// already paid the ticket's own object address — an outbid/early-settle
/// refund (reclaim) or the won tokens (redeem). The coin AT the ticket
/// address is the on-chain burn proof, so with nothing parked there the
/// ticket is still live and the pass skips it; a burn can therefore
/// never drop the escrowed value out of NAV.
async fn crank_bid_tickets(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
) {
    let Some(oa) = ctx.options_adapter_pkg else { return };
    for p in &holdings.positions {
        let PositionInfo::BidTicket {
            id,
            escrow_type,
            win_type,
            auction_id,
            escrow_amount,
            win_amount,
        } = p
        else {
            continue;
        };
        let result: Result<()> = async {
            let client = &wrap.client;
            let owner = SuiAddress::from(*id);
            let objects = client
                .owned_objects(owner, 20)
                .await
                .context("listing bid-ticket-address objects")?;
            // The auction's payout, if it landed: the win (pinned type,
            // at least the pinned amount) or the refund (exact escrow).
            let mut won = None;
            let mut refunded = None;
            for obj in &objects {
                // Only coins carry a payout; the type parameter is the asset.
                let Some(coin) = obj.as_coin_maybe() else { continue };
                let Some(tag) = obj.struct_tag() else { continue };
                let Some(inner) = tag.type_params.first() else { continue };
                let coin_type = protocol_types::asset::canonicalize_move_type(
                    &inner.to_canonical_string(/* with_prefix */ true),
                );
                let balance = coin.value();
                if coin_type == *win_type && balance >= *win_amount {
                    won = Some(obj.compute_object_reference());
                } else if coin_type == *escrow_type && balance == *escrow_amount {
                    refunded = Some(obj.compute_object_reference());
                }
            }
            if won.is_none() && refunded.is_none() {
                return Ok(()); // still live in the auction
            }

            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let ireg =
                pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
            let ticket_arg = pt.pure(id)?;
            let (label, action) = if let Some(re) = won {
                let receiving = pt.obj(ObjectArg::Receiving(re))?;
                pt.programmable_move_call(
                    oa,
                    Identifier::new("options_adapter").unwrap(),
                    Identifier::new("redeem_won_ticket").unwrap(),
                    vec![TypeTag::from_str(win_type)?],
                    vec![vault, ireg, ticket_arg, receiving],
                );
                ("options_adapter::redeem_won_ticket", "won bid ticket redeemed")
            } else {
                let re = refunded.expect("checked above");
                // Prefer the strict variant while the auction object
                // still exists (asserts the vault is no longer the best
                // bidder); after settle deletes it, the exact-refund
                // check alone gates the burn.
                match object_type_of(client, *auction_id).await.ok() {
                    Some(auction_ty) => {
                        let auction =
                            pt.obj(shared_object_arg(client, *auction_id, false).await?)?;
                        let receiving = pt.obj(ObjectArg::Receiving(re))?;
                        pt.programmable_move_call(
                            oa,
                            Identifier::new("options_adapter").unwrap(),
                            Identifier::new("reclaim_outbid_ticket").unwrap(),
                            bucket_type_args(&auction_ty)?,
                            vec![vault, ireg, ticket_arg, auction, receiving],
                        );
                    }
                    None => {
                        let receiving = pt.obj(ObjectArg::Receiving(re))?;
                        pt.programmable_move_call(
                            oa,
                            Identifier::new("options_adapter").unwrap(),
                            Identifier::new("reclaim_refunded_ticket").unwrap(),
                            vec![TypeTag::from_str(escrow_type)?],
                            vec![vault, ireg, ticket_arg, receiving],
                        );
                    }
                }
                ("options_adapter::reclaim_bid_ticket", "outbid bid ticket reclaimed")
            };
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, label).await?;
            info!(vault = %vault_id, ticket = %id, action, "bid ticket burned");
            Ok(())
        }
        .await;
        if let Err(e) = result {
            classify_and_log(vault_id, &e);
        }
    }
}

/// Crank 2: redeem positions on expired buckets.
async fn redeem_expired_positions(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    now_ms: u64,
) {
    for p in &holdings.positions {
        let PositionInfo::OptionPosition {
            id, bucket_id, is_put, underlying, settlement, call_type, via_vault_mm,
        } = p
        else {
            continue;
        };
        let result: Result<()> = async {
            let client = &wrap.client;
            let expiry = as_u64(&json_field(client, *bucket_id, "/expiry_ms").await?)
                .unwrap_or(u64::MAX);
            if now_ms < expiry {
                return Ok(());
            }
            let (pkg, module) = if *via_vault_mm {
                (ctx.trading_vault_pkg, "vault_mm")
            } else {
                (
                    ctx.options_adapter_pkg
                        .ok_or_else(|| anyhow!("options adapter unavailable"))?,
                    "options_adapter",
                )
            };
            let function = if *is_put { "redeem_put_position" } else { "redeem_call_position" };
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
            let bucket = pt.obj(shared_object_arg(client, *bucket_id, true).await?)?;
            let pos = pt.pure(id)?;
            let clock = clock_arg(&mut pt)?;
            pt.programmable_move_call(
                pkg,
                Identifier::new(module).unwrap(),
                Identifier::new(function).unwrap(),
                vec![
                    TypeTag::from_str(underlying)?,
                    TypeTag::from_str(settlement)?,
                    TypeTag::from_str(call_type)?,
                ],
                vec![vault, ireg, bucket, pos, clock],
            );
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::redeem").await?;
            info!(vault = %vault_id, position = %id, "expired position redeemed");
            Ok(())
        }
        .await;
        if let Err(e) = result {
            classify_and_log(vault_id, &e);
        }
    }
}

/// Crank 3: sweep settled amounts on every custody's active pools.
async fn sweep_custody_settled(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
) {
    let Some(dba) = ctx.deepbook_adapter_pkg else { return };
    for p in &holdings.positions {
        let PositionInfo::DeepBookCustody { id, pools, .. } = p else { continue };
        if pools.is_empty() {
            continue;
        }
        let result: Result<()> = async {
            let client = &wrap.client;
            let build = || async {
                let mut pt = ProgrammableTransactionBuilder::new();
                let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
                let ireg =
                    pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
                for (pool_id, base, quote) in pools {
                    let custody = pt.pure(id)?;
                    let pool = pt.obj(shared_object_arg(client, *pool_id, true).await?)?;
                    pt.programmable_move_call(
                        dba,
                        Identifier::new("deepbook_adapter").unwrap(),
                        Identifier::new("crank_withdraw_settled").unwrap(),
                        vec![TypeTag::from_str(base)?, TypeTag::from_str(quote)?],
                        vec![vault, ireg, custody, pool],
                    );
                }
                Ok::<_, anyhow::Error>(pt)
            };
            // Preflight with a FREE dev-inspect: a custody whose orders
            // are still resting reverts this crank on-chain every tick,
            // and paying gas for a guaranteed revert 4×/min per vault
            // drained the shared wallet overnight (2026-08-04). Only
            // submit what would actually execute.
            let probe = client.dev_inspect_ptb(wrap.signer.address, build().await?).await?;
            {
                use sui_types::effects::TransactionEffectsAPI;
                let status = probe.transaction.effects.status();
                if status.is_err() {
                    debug!(vault = %vault_id, status = ?status, "settle crank would revert; skipping");
                    return Ok(());
                }
            }
            submit_ptb(
                client,
                &wrap.signer,
                build().await?,
                ctx.gas_budget,
                "deepbook_adapter::crank_settle",
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            classify_and_log(vault_id, &e);
        }
    }
}

/// Crank 4: receive vault_mm transfer-ins parked at the vault address.
async fn sweep_vault_address(wrap: &SuiClientWrapper, ctx: &TradingVaultCtx, vault_id: ObjectID) {
    let result: Result<()> = async {
        let client = &wrap.client;
        let owner = SuiAddress::from(vault_id);
        let data = client
            .owned_objects(owner, 20)
            .await
            .context("listing vault-address objects")?;
        if data.is_empty() {
            return Ok(());
        }
        let position_type = format!("{}::position::Position", ctx.core_pkg);
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
        let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
        let mut count = 0usize;
        for obj in &data {
            let Some(tag) = obj.struct_tag() else { continue };
            let t = tag.to_canonical_string(/* with_prefix */ true);
            let receiving = pt.obj(ObjectArg::Receiving(obj.compute_object_reference()))?;
            if t == position_type {
                pt.programmable_move_call(
                    ctx.trading_vault_pkg,
                    Identifier::new("vault_mm").unwrap(),
                    Identifier::new("receive_mm_position").unwrap(),
                    vec![],
                    vec![vault, ireg, receiving],
                );
            } else if let Some(inner) = t
                .strip_prefix("0x2::coin::Coin<")
                .or_else(|| t.split_once("::coin::Coin<").map(|(_, r)| r))
            {
                let coin_type =
                    protocol_types::asset::canonicalize_move_type(inner.strip_suffix('>').unwrap_or(inner));
                let tag = TypeTag::from_str(&coin_type)?;
                let function = if ctx.feeds.contains_key(&coin_type)
                    || coin_type == holdings_deposit_hint()
                {
                    "receive_mm_coin"
                } else {
                    "receive_mm_option_coin"
                };
                pt.programmable_move_call(
                    ctx.trading_vault_pkg,
                    Identifier::new("vault_mm").unwrap(),
                    Identifier::new(function).unwrap(),
                    vec![tag],
                    vec![vault, ireg, receiving],
                );
            } else {
                continue;
            };
            count += 1;
        }
        if count == 0 {
            return Ok(());
        }
        submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "vault_mm::sweep").await?;
        info!(vault = %vault_id, count, "vault_mm transfer-ins swept");
        Ok(())
    }
    .await;
    if let Err(e) = result {
        classify_and_log(vault_id, &e);
    }
}

// receive_mm_coin routing has no per-vault deposit context here; catalog
// membership is the discriminator and this hint keeps the closure simple.
fn holdings_deposit_hint() -> String {
    String::new()
}

/// Crank 5: force-unwind when the oldest queue head is starved past
/// grace. v2: the age trigger is the OLDEST head across BOTH lanes —
/// lowest global sequence, which `run` is ordered by — matching the
/// on-chain `begin_force_session` unlock (a class-blocked junior head
/// still counts as unmet exit demand). The vault input is mutable, as v2
/// `begin_force_session(&mut vault, …)` requires.
async fn force_unwind_if_starved(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    view: &VaultView,
    run: &[QueueEntry],
    now_ms: u64,
) {
    let Some(dba) = ctx.deepbook_adapter_pkg else { return };
    let result: Result<()> = async {
        let client = &wrap.client;
        let Some(oldest) = run.first() else { return Ok(()) };
        if now_ms.saturating_sub(oldest.requested_at_ms) <= view.unwind_grace_ms {
            return Ok(());
        }
        // Starved: cancel every custody's books, then sweep balances home.
        for p in &holdings.positions {
            let PositionInfo::DeepBookCustody { id, assets, pools } = p else { continue };
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
            let clock = clock_arg(&mut pt)?;
            for (pool_id, base, quote) in pools {
                let custody = pt.pure(id)?;
                let pool = pt.obj(shared_object_arg(client, *pool_id, true).await?)?;
                pt.programmable_move_call(
                    dba,
                    Identifier::new("deepbook_adapter").unwrap(),
                    Identifier::new("force_cancel_all").unwrap(),
                    vec![TypeTag::from_str(base)?, TypeTag::from_str(quote)?],
                    vec![vault, ireg, custody, pool, clock],
                );
            }
            let mut sweep_types: Vec<&String> = assets.iter().collect();
            let deposit = holdings.deposit_type.clone();
            if !assets.contains(&deposit) {
                sweep_types.push(&holdings.deposit_type);
            }
            for asset in sweep_types {
                let custody = pt.pure(id)?;
                pt.programmable_move_call(
                    dba,
                    Identifier::new("deepbook_adapter").unwrap(),
                    Identifier::new("force_sweep").unwrap(),
                    vec![TypeTag::from_str(asset)?],
                    vec![vault, ireg, custody, clock],
                );
            }
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "deepbook_adapter::force_unwind")
                .await?;
            warn!(vault = %vault_id, custody = %id, "queue starved past grace — force-unwound custody");
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        classify_and_log(vault_id, &e);
    }
}

fn as_u64_ref(v: &Value) -> Option<u64> {
    as_u64(v)
}

/// The EquityBook's poster guardrails: (min_interval_ms, max_delta_bps).
async fn equity_book_params(client: &ChainClient, book_id: ObjectID) -> Result<(u64, u64)> {
    let min_interval = as_u64(&json_field(client, book_id, "/min_interval_ms").await?)
        .ok_or_else(|| anyhow!("EquityBook min_interval_ms unreadable"))?;
    let max_delta = as_u64(&json_field(client, book_id, "/max_delta_bps").await?)
        .ok_or_else(|| anyhow!("EquityBook max_delta_bps unreadable"))?;
    Ok((min_interval, max_delta))
}

/// Read-only reconciliation monitor (SO-299): recorded exposure vs the
/// attested equity mark, from the indexer view. Divergence past the
/// tolerance — in either direction — or a missing/stale mark while
/// exposure is open raises `hedge-reconciliation`.
fn monitor_external(ctx: &TradingVaultCtx, vault_id: ObjectID, ext: &ExternalView, now_ms: u64) {
    metrics::gauge!("keeper_external_exposure", "vault" => vault_id.to_string())
        .set(ext.exposure as f64);
    if let Some(eq) = ext.equity {
        metrics::gauge!("keeper_external_equity", "vault" => vault_id.to_string()).set(eq as f64);
        let deviation = eq.abs_diff(ext.exposure);
        if (deviation as u128) * 10_000
            > (ext.exposure as u128) * (ctx.reconciliation_tolerance_bps as u128)
        {
            tracing::error!(
                alert_id = "hedge-reconciliation",
                vault = %vault_id,
                exposure = ext.exposure,
                equity = eq,
                "external account equity diverges from recorded exposure"
            );
        }
    }
    if ext.exposure > 0 {
        let stale = match ext.equity_updated_at_ms {
            Some(t) => now_ms.saturating_sub(t) > ctx.equity_stale_alert_ms,
            None => true,
        };
        if ext.equity.is_none() || stale {
            tracing::error!(
                alert_id = "hedge-reconciliation",
                vault = %vault_id,
                exposure = ext.exposure,
                equity = ext.equity,
                updated_at_ms = ext.equity_updated_at_ms,
                "external exposure open but the equity mark is missing or stale"
            );
        }
    }
}

/// Crank 6: step the vault's EquityBook entry toward the venue-reported
/// target, within the on-chain guardrails (`crate::venue_equity`). The
/// keeper's wallet must be an allowlisted poster
/// (`equity_oracle::add_poster`); a denied post aborts E_NOT_POSTER (1)
/// → classified retry (alert). E_TOO_SOON (3) races are benign.
async fn post_external_equity(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    ext: &ExternalView,
    book_params: Option<(u64, u64)>,
    now_ms: u64,
) {
    let Some(target) = ctx.equity_source.equity_for(vault_id, ext.account) else {
        return;
    };
    let result: Result<()> = async {
        let (Some(pkg), Some(book_id)) = (ctx.equity_oracle_pkg, ctx.equity_book_id) else {
            warn!(vault = %vault_id, "equity target set but the equity-oracle package/book is unresolved; skipping post");
            return Ok(());
        };
        let Some((min_interval_ms, max_delta_bps)) = book_params else {
            // Params were unreadable this tick (already logged in tick()).
            return Ok(());
        };
        // The indexer view only knows entries that have been POSTED; a
        // bootstrap anchor (crank 6b's `init_entry`, SO-310) is invisible to
        // it, so a missing mark reads as the zero anchor it is. `post_equity`
        // waives the delta band for the first move off zero — no admin
        // `seed_equity` on the critical path. With no entry at all the post
        // aborts E_NOT_SEEDED (2) and alerts, which is the honest signal.
        let previous = ext.equity.unwrap_or(0);
        let updated_at = ext.equity_updated_at_ms.unwrap_or(0);
        if now_ms.saturating_sub(updated_at) < min_interval_ms {
            debug!(vault = %vault_id, "within the EquityBook min interval; next tick");
            return Ok(());
        }
        let clamped = clamp_step(previous, target, max_delta_bps);
        if clamped == previous {
            debug!(vault = %vault_id, previous, target, "no postable equity step within the guardrails");
            return Ok(());
        }
        let client = &wrap.client;
        let mut pt = ProgrammableTransactionBuilder::new();
        let book = pt.obj(shared_object_arg(client, book_id, true).await?)?;
        let vid = pt.pure(vault_id)?;
        let amount = pt.pure(clamped)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            pkg,
            Identifier::new("equity_oracle").unwrap(),
            Identifier::new("post_equity").unwrap(),
            vec![],
            vec![book, vid, amount, clock],
        );
        submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "equity_oracle::post_equity").await?;
        info!(vault = %vault_id, previous, posted = clamped, target, "external equity posted");
        Ok(())
    }
    .await;
    if let Err(e) = result {
        classify_and_log(vault_id, &e);
    }
}

/// Crank 6b (SO-310): create the vault's zero `EquityBook` entry while its
/// external account is still unfunded. `equity_oracle::init_entry` is
/// permissionless (the keeper pays gas) and exists exactly for this window:
/// once the curator's first release opens exposure, appraisals REQUIRE the
/// equity leg, and `record` on an entryless book aborts E_NOT_SEEDED until
/// an admin `seed_equity`. Both of `init_entry`'s guards — the entry already
/// exists, exposure already opened — are races we can lose to another
/// cranker or to the curator's own release PTB, which prepends the same
/// call; a lost race is benign.
async fn init_external_entry(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
) {
    if holdings.external_exposure > 0 {
        return;
    }
    let (Some(pkg), Some(book_id)) = (ctx.equity_oracle_pkg, ctx.equity_book_id) else {
        return;
    };
    let expected =
        protocol_types::asset::canonicalize_move_type(&format!("{pkg}::equity_oracle::EquityOracle"));
    if holdings.external_equity_oracle.as_deref() != Some(expected.as_str()) {
        return;
    }
    let result: Result<()> = async {
        let client = &wrap.client;
        if equity_book_has_entry(client, wrap.signer.address, pkg, book_id, vault_id).await? {
            return Ok(());
        }
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
        let book = pt.obj(shared_object_arg(client, book_id, true).await?)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            pkg,
            Identifier::new("equity_oracle").unwrap(),
            Identifier::new("init_entry").unwrap(),
            vec![],
            vec![vault, book, clock],
        );
        submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "equity_oracle::init_entry").await?;
        info!(vault = %vault_id, "created the vault's zero EquityBook entry");
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let msg = format!("{e:#}");
        if msg.contains("MoveAbort") {
            debug!(vault = %vault_id, error = %msg, "init_entry lost a race; next tick");
        } else {
            classify_and_log(vault_id, &e);
        }
    }
}

/// `equity_oracle::has_entry(book, vault_id)` via devInspect — no tx, no gas.
async fn equity_book_has_entry(
    client: &ChainClient,
    sender: SuiAddress,
    pkg: ObjectID,
    book_id: ObjectID,
    vault_id: ObjectID,
) -> Result<bool> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let book = pt.obj(shared_object_arg(client, book_id, false).await?)?;
    let vid = pt.pure(vault_id)?;
    pt.programmable_move_call(
        pkg,
        Identifier::new("equity_oracle").unwrap(),
        Identifier::new("has_entry").unwrap(),
        vec![],
        vec![book, vid],
    );
    let resp = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("devInspect equity_oracle::has_entry")?;
    sui_tx::chain::decode_return_value(&resp, 0).context("decoding has_entry bool return")
}

/// How long a fetched `/oracle/descriptor` stays authoritative. Short
/// enough that a provider flip reaches the composer within one tick or
/// two; long enough not to add a round trip per appraisal.
const DESCRIPTOR_TTL_MS: u64 = 30_000;

/// The live oracle descriptor, TTL-cached on the ctx (SO-346). No
/// stale-on-error fallback: composing legs against a provider that may
/// have just flipped is exactly the wrong failure mode, so an
/// unreachable oracle-service fails the crank (alerted, retried next
/// tick) instead.
async fn live_descriptor(ctx: &TradingVaultCtx) -> Result<oracle_client::OracleDescriptor> {
    {
        let cache = ctx.descriptor_cache.lock().expect("descriptor_cache poisoned");
        if let Some((at, d)) = cache.as_ref() {
            if at.elapsed() < std::time::Duration::from_millis(DESCRIPTOR_TTL_MS) {
                return Ok(d.clone());
            }
        }
    }
    let d = ctx
        .oracle
        .descriptor()
        .await
        .context("fetching /oracle/descriptor")?;
    *ctx.descriptor_cache.lock().expect("descriptor_cache poisoned") =
        Some((std::time::Instant::now(), d.clone()));
    Ok(d)
}

/// Compose the full attestation-bearing appraisal into `pt` and return
/// its Argument plus the per-asset attestation arguments (SO-370: the
/// mixed-asset fulfillment potato reuses them for `begin_fulfillment`'s
/// atts vector — `PriceAttestation` is `copy`). Shared by the
/// fulfillment crank and the mark-refresh crank. The price legs follow
/// the LIVE provider from `/oracle/descriptor` (SO-346): Pyth legs
/// resolve through Hermes + `PriceInfoObject`s, Switchboard legs through
/// `/oracle/legs`.
async fn compose_full_appraisal(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<(sui_types::transaction::Argument, BTreeMap<String, sui_types::transaction::Argument>)>
{
    let client = &wrap.client;
    let refs = refs_for(ctx, vault_id);

    // Option-coin types price via the options oracle; only the remaining
    // (underlying/settlement/plain) types need provider feeds.
    let needed = price_assets_needed(holdings, option_buckets);
    if needed.is_empty() {
        return compose_appraisal(client, pt, &refs, holdings, None, option_buckets, &[]).await;
    }
    let descriptor = live_descriptor(ctx).await?;
    match descriptor.provider {
        protocol_types::OracleProvider::Pyth => {
            let table = ctx
                .price_table
                .as_ref()
                .ok_or_else(|| anyhow!("price table unresolved — cannot appraise multi-asset vault"))?;
            // Optimistic: resolve legs for the feeds we HAVE; the composer
            // passes `none` for the rest and the chain aborts only if an
            // unpriced component is actually nonzero.
            let mut feeds = Vec::new();
            let mut price_infos: BTreeMap<String, ObjectID> = BTreeMap::new();
            let mut all_types: Vec<String> = needed.iter().cloned().collect();
            all_types.push(holdings.deposit_type.clone());
            for t in &all_types {
                let Some(feed) = ctx.feeds.get(t) else {
                    debug!(vault = %vault_id, asset = %t, "no pyth feed; passing none leg");
                    continue;
                };
                if !feeds.contains(feed) {
                    feeds.push(*feed);
                }
                let info = price_info_object_for(client, table, *feed).await?;
                price_infos.insert(t.clone(), info);
            }
            let (payloads, _) = pyth_client::latest_with_update_data(http, &ctx.hermes_url, &feeds)
                .await
                .context("fetching hermes update")?;
            let update = payloads
                .first()
                .ok_or_else(|| anyhow!("hermes returned no update payloads"))?;
            if payloads.len() > 1 {
                warn!(vault = %vault_id, payloads = payloads.len(), "hermes returned multiple payloads; using the first");
            }
            compose_appraisal(
                client,
                pt,
                &refs,
                holdings,
                Some(sui_tx::tx::oracle::OracleLegs::Pyth(sui_tx::tx::oracle::PythLegs {
                    adapter_pkg: ctx.oracle_pyth_pkg,
                    feed_registry_id: ctx.pyth_feed_registry_id,
                    handles: &ctx.pyth,
                    accumulator_update: update,
                    price_infos: &price_infos,
                    sender: wrap.signer.address,
                    gas_budget: ctx.gas_budget,
                })),
                option_buckets,
                &[],
            )
            .await
        }
        protocol_types::OracleProvider::Switchboard => {
            // Shared with the smoke and staging-mm-bot (SO-375): coverage
            // from the descriptor, signed payload from `/oracle/legs`.
            compose_switchboard_appraisal(
                client,
                pt,
                &refs,
                holdings,
                option_buckets,
                &descriptor,
                &ctx.oracle,
                &[],
            )
            .await
        }
    }
}

/// Max queue requests chained into one mixed fulfillment PTB (SO-370):
/// bounds the per-tick PTB size; longer runs drain over successive ticks.
const MAX_MIXED_RUN: usize = 25;

/// One pending withdrawal, read off the vault's tables.
struct QueueEntry {
    global_seq: u64,
    /// Wire lane code: 0 senior, 1 junior (untranched rides lane 1).
    lane: u8,
    /// Canonical payout asset.
    payout: String,
    requested_at_ms: u64,
    /// Junior request from a pre-reset generation: permanently
    /// zero-value, payable in any asset with no funding.
    wiped: bool,
}

/// Read one dynamic-field value JSON off a `Table<u64, V>` by deriving
/// the field id (no dynamic-field index API — some providers don't
/// serve one; same trick as the Pyth price_info lookup in
/// `discovery.rs`).
async fn table_entry_json(
    client: &ChainClient,
    table: ObjectID,
    key: u64,
) -> Result<Option<Value>> {
    let key_bytes = bcs::to_bytes(&key).context("bcs of table key")?;
    let field_id =
        sui_types::dynamic_field::derive_dynamic_field_id(table, &TypeTag::U64, &key_bytes)
            .context("deriving table entry field id")?;
    Ok(client
        .try_get_object_json(field_id)
        .await
        .with_context(|| format!("reading table entry {key}"))?
        .and_then(|(_, json)| json))
}

/// The pending run, merged across BOTH lanes in global-sequence order
/// (v2 §3.6): walk each lane's `entries` table (lane idx → global seq),
/// then fetch each request from the `requests` table. No payability
/// filtering here — `fulfill` skips the junior lane when risk-off, while
/// the force-unwind age trigger deliberately counts it.
async fn queue_run(
    client: &ChainClient,
    view: &VaultView,
    max: usize,
) -> Result<Vec<QueueEntry>> {
    let mut out = Vec::new();
    for (lane_code, lane) in [(0u8, &view.senior), (1u8, &view.junior)] {
        for idx in lane.head..lane.tail.min(lane.head + max as u64) {
            // Lane heads advance LAZILY on-chain (`lane_head_seq` walks
            // over holes) and `settle_queued_request` removes entries in
            // any order, so a missing index is a gap, not an error.
            let Some(seq) = table_entry_json(client, lane.entries_table, idx)
                .await?
                .and_then(|j| j.pointer("/value").and_then(as_u64_ref))
            else {
                continue;
            };
            // Missing request = a racing fulfill/settle consumed it
            // between the lane read and this one.
            let Some(req) = table_entry_json(client, view.requests_table, seq).await? else {
                continue;
            };
            // TypeName renders as a bare string in the gRPC json (struct
            // shape tolerated for renderings that don't collapse it).
            let payout = req
                .pointer("/value/payout_asset")
                .and_then(|v| v.as_str().or_else(|| v.pointer("/name").and_then(Value::as_str)))
                .map(protocol_types::asset::canonicalize_move_type)
                .ok_or_else(|| anyhow!("queue request {seq} missing payout_asset"))?;
            let requested_at_ms = req
                .pointer("/value/requested_at_ms")
                .and_then(as_u64_ref)
                .ok_or_else(|| anyhow!("queue request {seq} missing requested_at_ms"))?;
            let junior = req
                .pointer("/value/tranche")
                .and_then(|t| t.get("@variant"))
                .and_then(Value::as_str)
                == Some("Junior");
            let generation = req
                .pointer("/value/capital_generation")
                .and_then(as_u64_ref)
                .unwrap_or(0);
            out.push(QueueEntry {
                global_seq: seq,
                lane: lane_code,
                payout,
                requested_at_ms,
                wiped: junior && generation < view.active_junior_generation,
            });
        }
    }
    out.sort_by_key(|e| e.global_seq);
    out.truncate(max);
    Ok(out)
}

/// Crank 7: fulfillment with a full appraisal, over the merged two-lane
/// run. The junior lane is skipped entirely while the vault is risk-off
/// (the contract would refuse those heads anyway; the senior lane keeps
/// draining, §3.6). Accounting-payable heads (requested in the
/// accounting asset, aged past the grace fallback, or WIPED — zero-value
/// claims settle in any asset) keep the on-chain batch crank
/// `fulfill_withdrawals<Accounting>`; a head requesting a NON-accounting
/// payout goes through the fulfillment potato (SO-370) with one
/// attestation per distinct non-accounting payout asset in the reachable
/// run. The contract itself picks the lowest payable head per
/// `fulfill_next`, so the plan is a conservative (payout_type, count)
/// chain in global-sequence order; `fulfill_next` returning false is a
/// no-op, not an abort — an unfundable/wedged head is benign, never an
/// alert.
#[allow(clippy::too_many_arguments)]
async fn fulfill(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    view: &VaultView,
    run: &[QueueEntry],
    now_ms: u64,
) -> Result<()> {
    let client = &wrap.client;
    let refs = sui_tx::tx::trading_vault::TradingVaultRefs {
        package: ctx.trading_vault_pkg,
        vault_id,
        protocol_config_id: ctx.protocol_config_id,
        deposit_type: &holdings.deposit_type,
    };
    let accounting = &holdings.deposit_type;
    // Class-block: while risk-off, junior heads are unpayable (wiped ones
    // included — the contract checks the block first). Untranched vaults
    // ride lane 1 but are always Healthy, so this never skips them.
    let junior_blocked = view.risk_state != 0;
    let payable: Vec<&QueueEntry> =
        run.iter().filter(|e| !(junior_blocked && e.lane == 1)).collect();
    let Some(head) = payable.first() else {
        debug!(
            vault = %vault_id,
            risk_state = view.risk_state,
            "no payable lane heads (junior lane blocked, or the queue was drained under us)"
        );
        return Ok(());
    };
    let grace = view.unwind_grace_ms;
    let aged = |requested_at: u64| now_ms > requested_at.saturating_add(grace);

    if head.wiped || head.payout == *accounting || aged(head.requested_at_ms) {
        // Accounting-payable head: the on-chain batch crank drains every
        // consecutive accounting-payable head (grace-aged ones included).
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, _) =
            compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
                .await?;
        sui_tx::tx::trading_vault::build_fulfill_withdrawals(
            client,
            &mut pt,
            &refs,
            ctx.treasury_id,
            appraisal,
        )
        .await?;
        submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::fulfill_withdrawals")
            .await?;
        info!(vault = %vault_id, "trading-vault withdrawals fulfilled");
        return Ok(());
    }

    // Mixed path: plan the FIFO chain of payable requests. A request is
    // payable in its asset only when the vault holds it as a free
    // balance — which also guarantees the composed appraisal attested it
    // (free balances hard-require feeds). An unheld payout stops the run
    // there: chaining past it could never pay it (fulfill_next is
    // all-or-nothing per head), and a grace-aged unheld head falls to the
    // accounting branch above on a later tick.
    let mut plan: Vec<(String, usize)> = Vec::new();
    let mut non_accounting: std::collections::BTreeSet<String> = Default::default();
    for e in &payable {
        let p = if e.wiped
            || e.payout == *accounting
            || (aged(e.requested_at_ms) && !holdings.free_assets.contains(&e.payout))
        {
            accounting.clone()
        } else if holdings.free_assets.contains(&e.payout) {
            e.payout.clone()
        } else {
            break;
        };
        if p != *accounting {
            non_accounting.insert(p.clone());
        }
        match plan.last_mut() {
            Some((ty, count)) if *ty == p => *count += 1,
            _ => plan.push((p, 1)),
        }
    }
    if plan.is_empty() {
        // Head wants an asset the vault doesn't hold and isn't aged yet:
        // nothing fundable — benign, the requester can amend or the grace
        // fallback unwedges it.
        debug!(
            vault = %vault_id,
            payout = %head.payout,
            "queue head requests an unheld payout asset; nothing fundable this tick"
        );
        return Ok(());
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    let (appraisal, attestations) =
        compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
            .await?;
    let atts = non_accounting
        .iter()
        .map(|t| {
            attestations
                .get(t)
                .copied()
                .ok_or_else(|| anyhow!("no attestation composed for payout asset {t}"))
        })
        .collect::<Result<Vec<_>>>()?;
    sui_tx::tx::trading_vault::build_fulfill_mixed(
        client,
        &mut pt,
        &refs,
        ctx.treasury_id,
        appraisal,
        atts,
        &plan,
    )
    .await?;
    submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::fulfill_mixed").await?;
    info!(
        vault = %vault_id,
        requests = plan.iter().map(|(_, n)| n).sum::<usize>(),
        payout_assets = ?non_accounting,
        "mixed-asset withdrawal crank submitted"
    );
    Ok(())
}

/// Crank 8 (SO-304 → SO-418): the periodic capital crank — the same full
/// appraisal, finished with the permissionless `crank_capital`, which
/// beyond publishing fresh PositionAppraised / VaultAppraised marks runs
/// the full capital sync: hurdle accrual, the waterfall, risk-state
/// transition, and the curator-commitment test. Its
/// `mark_refresh_interval_ms` cadence is a correctness obligation on
/// tranched vaults (the accrual cap; see [`ACCRUAL_CAP_MS`]).
async fn crank_capital_pass(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<()> {
    let client = &wrap.client;
    let mut pt = ProgrammableTransactionBuilder::new();
    let (appraisal, _) =
        compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
            .await?;
    sui_tx::tx::trading_vault::build_crank_capital(
        client,
        &mut pt,
        &sui_tx::tx::trading_vault::TradingVaultRefs {
            package: ctx.trading_vault_pkg,
            vault_id,
            protocol_config_id: ctx.protocol_config_id,
            deposit_type: &holdings.deposit_type,
        },
        appraisal,
    )
    .await?;
    submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::crank_capital")
        .await?;
    info!(vault = %vault_id, "trading-vault capital synced (marks refreshed)");
    Ok(())
}

/// Crank 9 (SO-418, v2 §8.7): terminal settlement. Closed && unsettled →
/// drive the one-shot permissionless `snapshot_settlement` (a Closed
/// vault holds only the accounting asset, so the composed appraisal is
/// cheap), alerting `tv-settlement-missing` once it has been wedged for
/// over an hour. Closed && settled → permissionlessly
/// `settle_queued_request` every outstanding queued request (order no
/// longer matters — NAV is frozen). Settled with zero pending requests →
/// remember the vault as terminal so the tick loop never reads it again.
#[allow(clippy::too_many_arguments)]
async fn terminal_settlement(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    view: &VaultView,
    now_ms: u64,
) -> Result<()> {
    let client = &wrap.client;
    if !view.settled {
        {
            let mut watch =
                ctx.settlement_watch.lock().expect("settlement_watch poisoned");
            let (since, alerted) = *watch.entry(vault_id).or_insert((now_ms, false));
            if !alerted && now_ms.saturating_sub(since) > SETTLEMENT_MISSING_ALERT_MS {
                tracing::error!(
                    alert_id = "tv-settlement-missing",
                    vault = %vault_id,
                    unsettled_ms = now_ms.saturating_sub(since),
                    "vault is Closed but the settlement snapshot still hasn't landed"
                );
                watch.insert(vault_id, (since, true));
            }
        }
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, _) =
            compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
                .await?;
        sui_tx::tx::trading_vault::build_snapshot_settlement(
            client,
            &mut pt,
            &sui_tx::tx::trading_vault::TradingVaultRefs {
                package: ctx.trading_vault_pkg,
                vault_id,
                protocol_config_id: ctx.protocol_config_id,
                deposit_type: &holdings.deposit_type,
            },
            appraisal,
        )
        .await?;
        submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::snapshot_settlement")
            .await?;
        info!(vault = %vault_id, "terminal settlement snapshot taken");
        ctx.settlement_watch.lock().expect("settlement_watch poisoned").remove(&vault_id);
        return Ok(());
    }
    ctx.settlement_watch.lock().expect("settlement_watch poisoned").remove(&vault_id);

    let run = queue_run(client, view, MAX_MIXED_RUN).await?;
    if run.is_empty() {
        // Settled, nothing queued: terminal forever (Closed vaults accept
        // no new requests) — cache and never read this vault again.
        ctx.settled_done.lock().expect("settled_done poisoned").insert(vault_id);
        info!(vault = %vault_id, "vault settled with zero pending requests; retiring from the tick loop");
        return Ok(());
    }
    let refs = sui_tx::tx::trading_vault::TradingVaultRefs {
        package: ctx.trading_vault_pkg,
        vault_id,
        protocol_config_id: ctx.protocol_config_id,
        deposit_type: &holdings.deposit_type,
    };
    let mut pt = ProgrammableTransactionBuilder::new();
    for e in &run {
        sui_tx::tx::trading_vault::build_settle_queued_request(
            client,
            &mut pt,
            &refs,
            ctx.treasury_id,
            e.global_seq,
        )
        .await?;
    }
    submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::settle_queued_request")
        .await?;
    info!(vault = %vault_id, requests = run.len(), "queued requests settled from the pool");
    Ok(())
}

/// A Closed-but-unsettled vault is an incident after this long
/// (runbooks.md: 1h).
const SETTLEMENT_MISSING_ALERT_MS: u64 = 3_600_000;

async fn object_type_of(client: &ChainClient, id: ObjectID) -> Result<String> {
    client
        .get_object(id)
        .await?
        .struct_tag()
        .map(|t| t.to_canonical_string(/* with_prefix */ true))
        .ok_or_else(|| anyhow!("object {id} missing type"))
}

fn bucket_type_args(bucket_ty: &str) -> Result<Vec<TypeTag>> {
    let inner = bucket_ty
        .split_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .ok_or_else(|| anyhow!("unparseable bucket type {bucket_ty}"))?;
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '<' => { depth += 1; cur.push(c) }
            '>' => { depth -= 1; cur.push(c) }
            ',' if depth == 0 => { out.push(cur.trim().to_string()); cur.clear() }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.iter().map(|s| TypeTag::from_str(s).map_err(Into::into)).collect()
}

/// Boot helper: build the ctx from the token-info snapshot (preferring
/// the recorded governance block) + keeper config.
#[allow(clippy::too_many_arguments)]
pub async fn build_ctx(
    client: &ChainClient,
    snapshot: &token_info_client::Snapshot,
    treasury_id: ObjectID,
    core_protocol_config_id: ObjectID,
    gas_budget: u64,
    hermes_url: String,
    pyth: PythHandles,
    external: &crate::config::ExternalConfig,
    oracle: oracle_client::OracleClient,
    vol_window_days: u32,
    mark_refresh_interval_ms: u64,
) -> Result<Option<TradingVaultCtx>> {
    // Cadence SLO (SO-418): the hurdle accrual cap (2y) turns the crank
    // interval into a correctness bound — an interval anywhere near the
    // cap silently under-accrues senior hurdle. Fail fast on a config
    // that couldn't keep the required margin (interval must stay under
    // 1/1000 of the cap; the runtime alert fires at 1/100).
    anyhow::ensure!(
        mark_refresh_interval_ms < ACCRUAL_CAP_MS / 1000,
        "mark_refresh_interval_ms ({mark_refresh_interval_ms}) must be < ACCRUAL_CAP_MS/1000 \
         ({}) — the hurdle accrual cap makes the capital-crank cadence a correctness bound",
        ACCRUAL_CAP_MS / 1000,
    );
    // Absent records mean token-info served a partial snapshot (boot
    // race in a same-wave deploy) — every env publishes the family since
    // SO-292, so this is a loud boot failure, not a silent pass-disable:
    // crashing lets the supervisor retry against a warmed token-info.
    let tv = snapshot
        .trading_vault()
        .context("token-info snapshot has no tradingVault record (partial snapshot?)")?;
    let op = snapshot
        .oracle_pyth()
        .context("token-info snapshot has no oraclePyth record (partial snapshot?)")?;
    let trading_vault_pkg = tv.package().context("trading_vault package id")?;
    let oracle_pyth_pkg = op.package().context("oracle_pyth package id")?;

    let governance = if let Some(objs) = snapshot.trading_vault_objects() {
        GovernanceObjects {
            protocol_config_id: objs.vault_protocol_config()?,
            integration_registry_id: objs.integration_registry()?,
            oracle_registry_id: objs.oracle_registry()?,
            pyth_feed_registry_id: objs.pyth_feed_registry()?,
        }
    } else {
        discover_governance(client, &tv.publish_digest, &op.publish_digest)
            .await
            .context("discovering trading-vault governance objects")?
    };

    let mut feeds = BTreeMap::new();
    for token in snapshot.tokens() {
        if let Some(feed) = token.pyth_feed_id.as_deref() {
            if let Ok(id) = PriceFeedId::from_hex(feed) {
                feeds.insert(
                    protocol_types::asset::canonicalize_move_type(&token.coin_type),
                    id,
                );
            }
        }
    }
    let price_table = resolve_price_info_table_from(client, &pyth).await.ok();
    if price_table.is_none() {
        warn!("pyth price_info table unresolved; multi-asset fulfillment disabled");
    }

    // SO-299: equity-oracle package + its shared EquityBook — from the
    // deploy record when present, else the package's publish effects
    // (records written before the activation step recorded the book).
    let (equity_oracle_pkg, equity_book_id) = match snapshot.equity_oracle() {
        Some(eo) => {
            let pkg = eo.package().context("equity_oracle package id")?;
            let recorded = snapshot
                .trading_vault_objects()
                .map(|o| o.equity_book())
                .transpose()?
                .flatten();
            let book = match recorded {
                Some(id) => Some(id),
                None => {
                    created_of_types(client, &eo.publish_digest, &["equity_oracle::EquityBook"])
                        .await
                        .ok()
                        .and_then(|m| m.get("equity_oracle::EquityBook").copied())
                }
            };
            if book.is_none() {
                warn!("equity-oracle EquityBook not found in publish effects; external-equity legs disabled");
            }
            (Some(pkg), book)
        }
        None => (None, None),
    };
    // VolBook (premium marks): recorded id first, publish-effects scrape
    // as the fallback for pre-record deployments.
    let vol_book_id = {
        let recorded = snapshot
            .trading_vault_objects()
            .map(|o| o.vol_book())
            .transpose()?
            .flatten();
        match (recorded, snapshot.options_adapter()) {
            (Some(id), _) => Some(id),
            (None, Some(oa)) => {
                created_of_types(client, &oa.publish_digest, &["vol_book::VolBook"])
                    .await
                    .ok()
                    .and_then(|m| m.get("vol_book::VolBook").copied())
            }
            (None, None) => None,
        }
    };
    let equity_source: Box<dyn VenueEquitySource> = if let Some(b) = &external.bluefin {
        // SO-305: the Bluefin reader wins when configured; `[external.
        // equity_posts]` stays an operator/testing source for the other case.
        let mut accounts = BTreeMap::new();
        for (k, v) in &b.accounts {
            let vault = ObjectID::from_hex_literal(k)
                .with_context(|| format!("[external.bluefin.accounts] bad vault id {k:?}"))?;
            let account = SuiAddress::from_str(&v.account).map_err(|e| {
                anyhow!("[external.bluefin.accounts.{k}] bad account {:?}: {e}", v.account)
            })?;
            accounts.insert(
                vault,
                crate::venue_equity::BluefinVenueAccount {
                    account,
                    asset_decimals: v.asset_decimals,
                },
            );
        }
        Box::new(crate::venue_equity::Bluefin::spawn(
            b.base_url.clone(),
            accounts,
            std::time::Duration::from_millis(b.poll_interval_ms),
            std::time::Duration::from_millis(b.max_age_ms),
        ))
    } else if external.equity_posts.is_empty() {
        Box::new(crate::venue_equity::Disabled)
    } else {
        let mut targets = BTreeMap::new();
        for (k, amount) in &external.equity_posts {
            let id = ObjectID::from_hex_literal(k)
                .with_context(|| format!("[external.equity_posts] bad vault id {k:?}"))?;
            targets.insert(id, *amount);
        }
        Box::new(crate::venue_equity::Fixed::new(targets))
    };

    Ok(Some(TradingVaultCtx {
        trading_vault_pkg,
        oracle_pyth_pkg,
        deepbook_adapter_pkg: snapshot
            .deepbook_adapter()
            .map(|p| p.package())
            .transpose()?,
        options_adapter_pkg: snapshot
            .options_adapter()
            .map(|p| p.package())
            .transpose()?,
        exchange_adapter_pkg: snapshot
            .exchange_adapter()
            .map(|p| p.package())
            .transpose()?,
        core_pkg: snapshot.package().context("core package id")?,
        protocol_config_id: governance.protocol_config_id,
        integration_registry_id: governance.integration_registry_id,
        oracle_registry_id: governance.oracle_registry_id,
        pyth_feed_registry_id: governance.pyth_feed_registry_id,
        core_protocol_config_id,
        treasury_id,
        gas_budget,
        hermes_url,
        pyth,
        feeds,
        price_table,
        equity_oracle_pkg,
        equity_book_id,
        equity_source,
        vol_book_id,
        oracle,
        descriptor_cache: std::sync::Mutex::new(None),
        vol_window_days,
        reconciliation_tolerance_bps: external.reconciliation_tolerance_bps,
        equity_stale_alert_ms: external.equity_stale_alert_ms,
        mark_refreshed_at: std::sync::Mutex::new(BTreeMap::new()),
        mark_refresh_interval_ms,
        crank_backoff: std::sync::Mutex::new(BTreeMap::new()),
        capital_watch: std::sync::Mutex::new(BTreeMap::new()),
        settlement_watch: std::sync::Mutex::new(BTreeMap::new()),
        settled_done: std::sync::Mutex::new(std::collections::BTreeSet::new()),
    }))
}

