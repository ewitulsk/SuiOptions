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
//!   5. Force-unwind when the withdrawal queue head has aged past the
//!      vault's grace period: cancel books + sweep manager balances.
//!   6. Post external-account equity into the `EquityBook` (SO-299) when
//!      a venue source has an opinion, stepping within the book's
//!      on-chain guardrails ([`crate::venue_equity`]; the keeper wallet
//!      must be an allowlisted poster).
//!   7. Fulfill the withdrawal queue with a FULL attestation-bearing
//!      appraisal (sui_tx::tx::appraisal composer) — cash-only vaults
//!      need no price legs, everything else gets Pyth attestations;
//!      external-configured vaults get the mandatory equity leg.
//!   8. When nothing needs fulfilling but the vault holds positions or
//!      foreign assets, refresh their marks (SO-304): the same composed
//!      appraisal finished with the permissionless `crank_appraisal`,
//!      rate-limited per vault.
//!
//! Alongside the cranks, a read-only reconciliation monitor
//! (`hedge-reconciliation` alert) compares each external account's
//! recorded exposure against its attested equity every tick.
//!
//! Governance/object ids come from token-info's `trading_vault_objects`
//! block when present (written by the deploy-time activation, SO-292);
//! deployments that predate it fall back to publish-effects discovery.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use indexer_graphql::IndexerClient;
use serde_json::Value;
use sui_sdk::rpc_types::{
    ObjectChange, SuiObjectDataFilter, SuiObjectDataOptions, SuiObjectResponseQuery,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::ObjectArg;
use tracing::{debug, info, warn};

use protocol_types::PriceFeedId;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::appraisal::{
    compose_appraisal, discover_holdings, pyth_assets_needed, AppraisalRefs, DbmLegInfo,
    OptionBucketInfo, PositionInfo, PriceLegs, VaultHoldings,
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
    pub vol_window_days: u32,
    /// `hedge-reconciliation` thresholds (keeper config `[external]`).
    pub reconciliation_tolerance_bps: u64,
    pub equity_stale_alert_ms: u64,
    /// Per-vault trustless DBM equity legs (`[external.dbm]`, SO-299
    /// phase C): a listed vault's appraisal composes
    /// `dbm_oracle::record{,_no_debt}` instead of `equity_oracle::record`.
    pub dbm: BTreeMap<ObjectID, DbmLegInfo>,
    /// Per-vault last mark-refresh time (crank 8, SO-304).
    pub mark_refreshed_at: std::sync::Mutex<BTreeMap<ObjectID, u64>>,
}

/// Minimum spacing between per-vault mark-refresh cranks (SO-304): the
/// tick loop runs much faster than fresh marks are worth their gas.
const MARK_REFRESH_INTERVAL_MS: u64 = 300_000;

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
    client: &SuiClient,
    publish_digest: &str,
    wanted: &[&str],
) -> Result<BTreeMap<String, ObjectID>> {
    let digest = publish_digest.parse().context("parsing publish digest")?;
    let resp = client
        .read_api()
        .get_transaction_with_options(
            digest,
            SuiTransactionBlockResponseOptions::new()
                .with_object_changes()
                .with_effects(),
        )
        .await
        .context("fetching publish tx")?;
    let mut out = BTreeMap::new();
    // Prefer objectChanges; pruned nodes may only serve effects.
    if let Some(changes) = resp.object_changes {
        for change in changes {
            if let ObjectChange::Created { object_id, object_type, .. } = change {
                let key = format!("{}::{}", object_type.module, object_type.name);
                if wanted.iter().any(|w| key == *w) {
                    out.insert(key, object_id);
                }
            }
        }
    }
    if out.len() < wanted.len() {
        if let Some(effects) = resp.effects {
            use sui_sdk::rpc_types::SuiTransactionBlockEffectsAPI;
            let ids: Vec<ObjectID> =
                effects.created().iter().map(|c| c.reference.object_id).collect();
            let objs = client
                .read_api()
                .multi_get_object_with_options(ids, SuiObjectDataOptions::new().with_type())
                .await
                .context("resolving created objects")?;
            for o in objs {
                if let Some(data) = o.data {
                    if let Some(t) = data.type_ {
                        let full = t.to_string();
                        for w in wanted {
                            if full.ends_with(&format!("::{w}")) {
                                out.insert((*w).to_string(), data.object_id);
                            }
                        }
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
    client: &SuiClient,
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
        if v.state == "closed" && v.pending_withdrawals == 0 {
            continue;
        }
        let vault_id = match ObjectID::from_hex_literal(&v.vault_id.to_hex()) {
            Ok(id) => id,
            Err(_) => continue,
        };
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
        if let Err(e) = tick_one(
            wrap,
            http,
            ctx,
            vault_id,
            v.pending_withdrawals,
            &option_buckets,
            external.as_ref(),
            book_params,
        )
        .await
        {
            classify_and_log(vault_id, &e);
        }
    }
}

fn classify_and_log(vault_id: ObjectID, e: &anyhow::Error) {
    let msg = format!("{e:#}");
    // Known benign shapes: appraisal raced a session (83), incomplete
    // because holdings changed under us (82), insufficient free balance
    // (78), auction not yet past deadline / bucket state races.
    let benign = [", 82)", ", 83)", ", 78)", "deadline", "not expired"];
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
    // (vault position_missing, 86), or the auction/receiving input was
    // consumed between compose and execute.
    let bid_ticket_race = (msg.contains("options_adapter") && msg.contains(", 10)"))
        || (msg.contains("vault") && msg.contains(", 86)"))
        || msg.contains("not available for consumption");
    if benign.iter().any(|b| msg.contains(b)) || equity_race || vol_race || bid_ticket_race {
        debug!(vault = %vault_id, error = %msg, "trading-vault crank lost a race; next tick");
    } else {
        tracing::error!(
            alert_id = "tx-failed-keeper",
            vault = %vault_id,
            class = "retry",
            error = %msg,
            "trading-vault crank failed; retrying next tick"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn tick_one(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    pending_withdrawals: u64,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    external: Option<&ExternalView>,
    book_params: Option<(u64, u64)>,
) -> Result<()> {
    let client = &wrap.client;
    let holdings = discover_holdings(client, vault_id).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    settle_due_tickets(wrap, ctx, vault_id, &holdings, now_ms).await;
    crank_bid_tickets(wrap, ctx, vault_id, &holdings).await;
    redeem_expired_positions(wrap, ctx, vault_id, &holdings, now_ms).await;
    sweep_custody_settled(wrap, ctx, vault_id, &holdings).await;
    sweep_vault_address(wrap, ctx, vault_id).await;
    force_unwind_if_starved(wrap, ctx, vault_id, &holdings, now_ms).await;
    if let Some(ext) = external {
        // BEFORE fulfillment so its equity leg reads a fresh mark.
        post_external_equity(wrap, ctx, vault_id, ext, book_params, now_ms).await;
    }

    if pending_withdrawals > 0 {
        // Re-discover: the cranks above may have changed holdings.
        let holdings = discover_holdings(client, vault_id).await?;
        fulfill(wrap, http, ctx, vault_id, &holdings, option_buckets).await?;
    } else if !holdings.is_cash_only() && mark_refresh_due(ctx, vault_id, now_ms) {
        // Crank 8 (SO-304): nothing to fulfill, but the vault holds
        // positions / foreign assets — refresh their marks. Cash-only
        // vaults have nothing to mark and are skipped. Re-discover:
        // the cranks above may have changed holdings.
        let holdings = discover_holdings(client, vault_id).await?;
        if !holdings.is_cash_only() {
            refresh_marks(wrap, http, ctx, vault_id, &holdings, option_buckets).await?;
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
    now_ms.saturating_sub(last) >= MARK_REFRESH_INTERVAL_MS
}

fn refs_for(ctx: &TradingVaultCtx, vault_id: ObjectID) -> AppraisalRefs {
    AppraisalRefs {
        trading_vault_pkg: ctx.trading_vault_pkg,
        oracle_pyth_pkg: ctx.oracle_pyth_pkg,
        deepbook_adapter_pkg: ctx.deepbook_adapter_pkg,
        options_adapter_pkg: ctx.options_adapter_pkg,
        vault_id,
        protocol_config_id: ctx.protocol_config_id,
        oracle_registry_id: ctx.oracle_registry_id,
        pyth_feed_registry_id: ctx.pyth_feed_registry_id,
        equity_oracle_pkg: ctx.equity_oracle_pkg,
        equity_book_id: ctx.equity_book_id,
        vol_book_id: ctx.vol_book_id,
        dbm: ctx.dbm.get(&vault_id).cloned(),
    }
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
async fn vol_book_meta(client: &SuiClient, book_id: ObjectID) -> Result<(u64, u64, ObjectID)> {
    let min_interval = as_u64(&json_field(client, book_id, "/fields/min_interval_ms").await?)
        .ok_or_else(|| anyhow!("VolBook min_interval_ms unreadable"))?;
    let max_delta = as_u64(&json_field(client, book_id, "/fields/max_delta_bps").await?)
        .ok_or_else(|| anyhow!("VolBook max_delta_bps unreadable"))?;
    let table = json_field(client, book_id, "/fields/entries/fields/id/id").await?;
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
    client: &SuiClient,
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
    let resp = client
        .read_api()
        .get_object_with_options(
            field_id,
            sui_json_rpc_types::SuiObjectDataOptions::new(),
        )
        .await
        .context("reading VolBook entry")?;
    let Some(data) = resp.data else {
        return Ok(None);
    };
    let vol = as_u64(&json_field(client, data.object_id, "/fields/value/fields/vol_bps").await?)
        .ok_or_else(|| anyhow!("VolBook entry vol_bps unreadable"))?;
    let at =
        as_u64(&json_field(client, data.object_id, "/fields/value/fields/updated_at_ms").await?)
            .ok_or_else(|| anyhow!("VolBook entry updated_at_ms unreadable"))?;
    Ok(Some((vol, at)))
}

async fn json_field(client: &SuiClient, id: ObjectID, pointer: &str) -> Result<Value> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_content())
        .await?;
    let content = resp
        .data
        .and_then(|d| d.content)
        .ok_or_else(|| anyhow!("object {id} missing content"))?;
    let json = serde_json::to_value(content)?;
    json.pointer(pointer)
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
                &json_field(client, *auction_id, "/fields/deadline_ms").await?,
            )
            .ok_or_else(|| anyhow!("auction missing deadline"))?;
            let bucket_ty = object_type_of(client, *bucket_id).await?;
            let expiry = as_u64(&json_field(client, *bucket_id, "/fields/expiry_ms").await?)
                .unwrap_or(u64::MAX);
            let invalidated = json_field(client, *bucket_id, "/fields/invalidated")
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
            let page = client
                .read_api()
                .get_owned_objects(
                    owner,
                    Some(SuiObjectResponseQuery::new(
                        None,
                        Some(SuiObjectDataOptions::new().with_type().with_content()),
                    )),
                    None,
                    Some(20),
                )
                .await
                .context("listing bid-ticket-address objects")?;
            // The auction's payout, if it landed: the win (pinned type,
            // at least the pinned amount) or the refund (exact escrow).
            let mut won = None;
            let mut refunded = None;
            for obj in &page.data {
                let Some(d) = obj.data.as_ref() else { continue };
                let Some(t) = d.type_.as_ref().map(|t| t.to_string()) else { continue };
                let Some(inner) = t
                    .strip_prefix("0x2::coin::Coin<")
                    .or_else(|| t.split_once("::coin::Coin<").map(|(_, r)| r))
                else {
                    continue;
                };
                let coin_type =
                    protocol_types::asset::canonicalize_move_type(inner.trim_end_matches('>'));
                let balance = d
                    .content
                    .as_ref()
                    .and_then(|c| serde_json::to_value(c).ok())
                    .and_then(|j| j.pointer("/fields/balance").and_then(as_u64_ref))
                    .unwrap_or(0);
                if coin_type == *win_type && balance >= *win_amount {
                    won = Some((d.object_id, d.version, d.digest));
                } else if coin_type == *escrow_type && balance == *escrow_amount {
                    refunded = Some((d.object_id, d.version, d.digest));
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
            let expiry = as_u64(&json_field(client, *bucket_id, "/fields/expiry_ms").await?)
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
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
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
            submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "deepbook_adapter::crank_settle")
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
        let page = client
            .read_api()
            .get_owned_objects(
                owner,
                Some(SuiObjectResponseQuery::new(
                    Some(SuiObjectDataFilter::MatchAny(vec![])),
                    Some(SuiObjectDataOptions::new().with_type()),
                )),
                None,
                Some(20),
            )
            .await;
        // MatchAny(vec![]) semantics differ across node versions; fall
        // back to no filter on error.
        let data = match page {
            Ok(p) => p.data,
            Err(_) => {
                client
                    .read_api()
                    .get_owned_objects(
                        owner,
                        Some(SuiObjectResponseQuery::new(
                            None,
                            Some(SuiObjectDataOptions::new().with_type()),
                        )),
                        None,
                        Some(20),
                    )
                    .await
                    .context("listing vault-address objects")?
                    .data
            }
        };
        if data.is_empty() {
            return Ok(());
        }
        let position_type = format!("{}::position::Position", ctx.core_pkg);
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
        let ireg = pt.obj(shared_object_arg(client, ctx.integration_registry_id, false).await?)?;
        let mut count = 0usize;
        for obj in &data {
            let Some(d) = obj.data.as_ref() else { continue };
            let Some(t) = d.type_.as_ref().map(|t| t.to_string()) else { continue };
            let receiving = pt.obj(ObjectArg::Receiving((d.object_id, d.version, d.digest)))?;
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
                    protocol_types::asset::canonicalize_move_type(inner.trim_end_matches('>'));
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

/// Crank 5: force-unwind when the queue head is starved past grace.
async fn force_unwind_if_starved(
    wrap: &SuiClientWrapper,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    now_ms: u64,
) {
    let Some(dba) = ctx.deepbook_adapter_pkg else { return };
    let result: Result<()> = async {
        let client = &wrap.client;
        let head = as_u64(&json_field(client, vault_id, "/fields/queue_head").await?)
            .unwrap_or(0);
        let tail = as_u64(&json_field(client, vault_id, "/fields/queue_tail").await?)
            .unwrap_or(0);
        if head >= tail {
            return Ok(());
        }
        let grace = as_u64(
            &json_field(client, vault_id, "/fields/config/fields/unwind_grace_ms").await?,
        )
        .unwrap_or(u64::MAX);
        let queue_table = json_field(client, vault_id, "/fields/queue/fields/id/id")
            .await?
            .as_str()
            .and_then(|s| ObjectID::from_hex_literal(s).ok())
            .ok_or_else(|| anyhow!("vault queue table id unreadable"))?;
        let entry = client
            .read_api()
            .get_dynamic_field_object(
                queue_table,
                sui_types::dynamic_field::DynamicFieldName {
                    type_: TypeTag::U64,
                    value: serde_json::json!(head.to_string()),
                },
            )
            .await
            .context("reading queue head")?;
        let requested_at = entry
            .data
            .and_then(|d| d.content)
            .and_then(|c| serde_json::to_value(c).ok())
            .and_then(|j| {
                j.pointer("/fields/value/fields/requested_at_ms")
                    .and_then(as_u64_ref)
            })
            .ok_or_else(|| anyhow!("queue head missing requested_at_ms"))?;
        if now_ms.saturating_sub(requested_at) <= grace {
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
async fn equity_book_params(client: &SuiClient, book_id: ObjectID) -> Result<(u64, u64)> {
    let min_interval = as_u64(&json_field(client, book_id, "/fields/min_interval_ms").await?)
        .ok_or_else(|| anyhow!("EquityBook min_interval_ms unreadable"))?;
    let max_delta = as_u64(&json_field(client, book_id, "/fields/max_delta_bps").await?)
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
        let Some(previous) = ext.equity else {
            warn!(
                vault = %vault_id,
                target,
                "no EquityBook entry for this vault — admin seed_equity required; skipping post"
            );
            return Ok(());
        };
        if previous == 0 && target > 0 {
            warn!(
                vault = %vault_id,
                target,
                "equity entry is zero — a poster cannot move it (bps-of-zero); admin seed_equity required"
            );
            return Ok(());
        }
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

/// Compose the full attestation-bearing appraisal into `pt` (Pyth legs
/// resolved through Hermes as needed) and return its Argument. Shared by
/// the fulfillment crank and the mark-refresh crank.
async fn compose_full_appraisal(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<sui_types::transaction::Argument> {
    let client = &wrap.client;
    let refs = refs_for(ctx, vault_id);

    // Option-coin types price via the options oracle; only the remaining
    // (underlying/settlement/plain) types need pyth feeds — plus the DBM
    // equity leg's base/quote for a dbm-configured vault.
    let needed = pyth_assets_needed(holdings, option_buckets, refs.dbm.as_ref());
    if needed.is_empty() {
        compose_appraisal(client, pt, &refs, holdings, None, option_buckets).await
    } else {
        let table = ctx
            .price_table
            .as_ref()
            .ok_or_else(|| anyhow!("price table unresolved — cannot appraise multi-asset vault"))?;
        // Optimistic: resolve legs for the feeds we HAVE; the composer
        // passes `none` for the rest and the chain aborts only if an
        // unpriced component is actually nonzero.
        let mut feeds = Vec::new();
        let mut price_infos = BTreeMap::new();
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
            Some(PriceLegs { pyth: &ctx.pyth, accumulator_update: update, price_infos: &price_infos }),
            option_buckets,
        )
        .await
    }
}

/// Crank 7: fulfillment with a full appraisal.
async fn fulfill(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<()> {
    let client = &wrap.client;
    let mut pt = ProgrammableTransactionBuilder::new();
    let appraisal =
        compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
            .await?;
    sui_tx::tx::trading_vault::build_fulfill_withdrawals(
        client,
        &mut pt,
        &sui_tx::tx::trading_vault::TradingVaultRefs {
            package: ctx.trading_vault_pkg,
            vault_id,
            protocol_config_id: ctx.protocol_config_id,
            deposit_type: &holdings.deposit_type,
        },
        ctx.treasury_id,
        appraisal,
    )
    .await?;
    submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::fulfill_withdrawals")
        .await?;
    info!(vault = %vault_id, "trading-vault withdrawals fulfilled");
    Ok(())
}

/// Crank 8 (SO-304): a periodic mark refresh — the same full appraisal,
/// finished with the permissionless `crank_appraisal` so the
/// PositionAppraised / VaultAppraised events publish fresh marks with no
/// deposit/fulfillment attached.
async fn refresh_marks(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<()> {
    let client = &wrap.client;
    let mut pt = ProgrammableTransactionBuilder::new();
    let appraisal =
        compose_full_appraisal(wrap, http, ctx, vault_id, holdings, option_buckets, &mut pt)
            .await?;
    sui_tx::tx::trading_vault::build_crank_appraisal(
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
    submit_ptb(client, &wrap.signer, pt, ctx.gas_budget, "trading_vault::crank_appraisal")
        .await?;
    info!(vault = %vault_id, "trading-vault marks refreshed");
    Ok(())
}

async fn object_type_of(client: &SuiClient, id: ObjectID) -> Result<String> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_type())
        .await?;
    resp.data
        .and_then(|d| d.type_)
        .map(|t| t.to_string())
        .ok_or_else(|| anyhow!("object {id} missing type"))
}

fn bucket_type_args(bucket_ty: &str) -> Result<Vec<TypeTag>> {
    let inner = bucket_ty
        .split_once('<')
        .map(|(_, rest)| rest.trim_end_matches('>'))
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
    client: &SuiClient,
    snapshot: &token_info_client::Snapshot,
    treasury_id: ObjectID,
    core_protocol_config_id: ObjectID,
    gas_budget: u64,
    hermes_url: String,
    pyth: PythHandles,
    external: &crate::config::ExternalConfig,
    oracle: oracle_client::OracleClient,
    vol_window_days: u32,
) -> Result<Option<TradingVaultCtx>> {
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
    // Trustless DBM equity legs (SO-299 phase C): per-vault manager
    // identity from `[external.dbm]`, package from the snapshot. Config
    // without a deployed dbm-oracle package is a boot error — the listed
    // vault's appraisals could never complete.
    let mut dbm: BTreeMap<ObjectID, DbmLegInfo> = BTreeMap::new();
    if !external.dbm.is_empty() {
        let dbm_oracle_pkg = snapshot
            .dbm_oracle()
            .map(|p| p.package())
            .transpose()?
            .context("[external.dbm] configured but token-info has no dbm_oracle package")?;
        for (k, v) in &external.dbm {
            let vault = ObjectID::from_hex_literal(k)
                .with_context(|| format!("[external.dbm] bad vault id {k:?}"))?;
            let id = |field: &str, s: &str| -> Result<ObjectID> {
                ObjectID::from_hex_literal(s)
                    .with_context(|| format!("[external.dbm.{k}] bad {field} {s:?}"))
            };
            let base_canonical = protocol_types::asset::canonicalize_move_type(&v.base_type);
            let quote_canonical = protocol_types::asset::canonicalize_move_type(&v.quote_type);
            // Venue assets aren't catalog tokens: register their feeds so
            // the composer can mint their attestation legs.
            feeds.insert(
                base_canonical.clone(),
                PriceFeedId::from_hex(&v.base_feed_id)
                    .map_err(|e| anyhow!("[external.dbm.{k}] bad base_feed_id: {e}"))?,
            );
            feeds.insert(
                quote_canonical.clone(),
                PriceFeedId::from_hex(&v.quote_feed_id)
                    .map_err(|e| anyhow!("[external.dbm.{k}] bad quote_feed_id: {e}"))?,
            );
            dbm.insert(
                vault,
                DbmLegInfo {
                    dbm_oracle_pkg,
                    margin_manager_id: id("margin_manager_id", &v.margin_manager_id)?,
                    deepbook_pool_id: id("deepbook_pool_id", &v.deepbook_pool_id)?,
                    base_margin_pool_id: id("base_margin_pool_id", &v.base_margin_pool_id)?,
                    quote_margin_pool_id: id("quote_margin_pool_id", &v.quote_margin_pool_id)?,
                    base_type: base_canonical,
                    quote_type: quote_canonical,
                },
            );
        }
    }
    let equity_source: Box<dyn VenueEquitySource> = if external.equity_posts.is_empty() {
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
        vol_window_days,
        reconciliation_tolerance_bps: external.reconciliation_tolerance_bps,
        equity_stale_alert_ms: external.equity_stale_alert_ms,
        dbm,
        mark_refreshed_at: std::sync::Mutex::new(BTreeMap::new()),
    }))
}
