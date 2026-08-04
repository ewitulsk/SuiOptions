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
//!      must be an allowlisted poster). While an external account is still
//!      unfunded, create its zero entry instead (SO-310, permissionless).
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
use sui_tx::chain::{created_objects, ChainClient};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, TransactionKind};
use tracing::{debug, info, warn};

use protocol_types::PriceFeedId;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::appraisal::{
    compose_appraisal, discover_holdings, price_assets_needed, AppraisalRefs,
    OptionBucketInfo, PositionInfo, VaultHoldings,
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
    // BEFORE the post so a freshly registered account has its zero anchor to
    // step off of.
    init_external_entry(wrap, ctx, vault_id, &holdings).await;
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
        deepbook_adapter_pkg: ctx.deepbook_adapter_pkg,
        options_adapter_pkg: ctx.options_adapter_pkg,
        vault_id,
        protocol_config_id: ctx.protocol_config_id,
        oracle_registry_id: ctx.oracle_registry_id,
        equity_oracle_pkg: ctx.equity_oracle_pkg,
        equity_book_id: ctx.equity_book_id,
        vol_book_id: ctx.vol_book_id,
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
            &json_field(client, vault_id, "/config/unwind_grace_ms").await?,
        )
        .unwrap_or(u64::MAX);
        let queue_table = json_field(client, vault_id, "/queue/id")
            .await?
            .as_str()
            .and_then(|s| ObjectID::from_hex_literal(s).ok())
            .ok_or_else(|| anyhow!("vault queue table id unreadable"))?;
        // Derive the field id rather than asking for a dynamic-field index
        // (some providers don't serve one) — same trick as the Pyth
        // price_info lookup in `discovery.rs`.
        let key_bytes = bcs::to_bytes(&head).context("bcs of queue head index")?;
        let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
            queue_table,
            &TypeTag::U64,
            &key_bytes,
        )
        .context("deriving queue head field id")?;
        let requested_at = client
            .try_get_object_json(field_id)
            .await
            .context("reading queue head")?
            .and_then(|(_, json)| json)
            .and_then(|j| j.pointer("/value/requested_at_ms").and_then(as_u64_ref))
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
/// its Argument. Shared by the fulfillment crank and the mark-refresh
/// crank. The price legs follow the LIVE provider from
/// `/oracle/descriptor` (SO-346): Pyth legs resolve through Hermes +
/// `PriceInfoObject`s, Switchboard legs through `/oracle/legs`.
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
    // (underlying/settlement/plain) types need provider feeds.
    let needed = price_assets_needed(holdings, option_buckets);
    if needed.is_empty() {
        return compose_appraisal(client, pt, &refs, holdings, None, option_buckets).await;
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
                })),
                option_buckets,
            )
            .await
        }
        protocol_types::OracleProvider::Switchboard => {
            let adapter = descriptor.adapter.as_ref().ok_or_else(|| {
                anyhow!(
                    "live provider {} has no adapter deployed on this network — cannot build price legs",
                    descriptor.provider
                )
            })?;
            let adapter_pkg = ObjectID::from_hex_literal(&adapter.adapter_package_id)
                .context("parsing descriptor adapter package id")?;
            let feed_registry_id = ObjectID::from_hex_literal(&adapter.feed_registry_id)
                .context("parsing descriptor feed registry id")?;

            // Same none-leg posture as the Pyth arm, with coverage from
            // the descriptor. The deposit asset's feed must ride along:
            // `attest<Asset, Dep>` crosses each asset against it inside
            // one `Quotes` bundle.
            let mut all_types: Vec<String> = needed.iter().cloned().collect();
            all_types.push(holdings.deposit_type.clone());
            let mut request: Vec<String> = Vec::new();
            let mut feed_hashes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for t in &all_types {
                let Some(hash) = descriptor.feeds.get(t) else {
                    debug!(vault = %vault_id, asset = %t, "no switchboard feed; passing none leg");
                    continue;
                };
                let bytes = hex::decode(hash.trim().trim_start_matches("0x"))
                    .with_context(|| format!("descriptor feed hash for {t} is not hex"))?;
                if bytes.len() != 32 {
                    return Err(anyhow!(
                        "descriptor feed hash for {t} is {} bytes; expected 32",
                        bytes.len()
                    ));
                }
                feed_hashes.insert(t.clone(), bytes);
                request.push(t.clone());
            }
            if request.is_empty() {
                return Err(anyhow!(
                    "no switchboard feed hash for any priced asset (deposit {})",
                    holdings.deposit_type
                ));
            }
            let legs = ctx
                .oracle
                .legs(&request)
                .await
                .context("fetching /oracle/legs")?;
            let oracle_client::OracleLegsResponse::Switchboard(sw) = legs else {
                return Err(anyhow!(
                    "/oracle/legs answered for a different provider than the descriptor — \
                     provider flipped mid-compose; retrying next tick"
                ));
            };
            let payload = switchboard_payload(&sw)?;
            let switchboard_pkg = ObjectID::from_hex_literal(&sw.switchboard_package_id)
                .context("parsing on_demand package id")?;
            compose_appraisal(
                client,
                pt,
                &refs,
                holdings,
                Some(sui_tx::tx::oracle::OracleLegs::Switchboard(
                    sui_tx::tx::oracle::SwitchboardLegs {
                        adapter_pkg,
                        feed_registry_id,
                        switchboard_pkg,
                        payload: &payload,
                        feed_hashes: &feed_hashes,
                    },
                )),
                option_buckets,
            )
            .await
        }
    }
}

/// `/oracle/legs` wire → the submit shape `run_N` takes. Object ids
/// parse here (the wire is string-typed for JS safety); arity/shape
/// checks stay in `SwitchboardQuotePayload::validate` at PTB build time.
fn switchboard_payload(
    sw: &oracle_client::SwitchboardLegsPayload,
) -> Result<sui_tx::tx::oracle::SwitchboardQuotePayload> {
    let q = &sw.quote;
    Ok(sui_tx::tx::oracle::SwitchboardQuotePayload {
        feed_ids: q.feed_id_bytes()?,
        values: q.values_u128()?,
        values_neg: q.values_neg.clone(),
        min_oracle_samples: q.min_oracle_samples.clone(),
        signatures: q.signature_bytes()?,
        slot: q.slot,
        timestamp_seconds: q.timestamp_seconds,
        oracle_ids: q
            .oracle_ids
            .iter()
            .map(|o| {
                ObjectID::from_hex_literal(o)
                    .with_context(|| format!("parsing oracle object id {o:?}"))
            })
            .collect::<Result<Vec<_>>>()?,
        queue_id: ObjectID::from_hex_literal(&sw.queue_id).context("parsing queue object id")?,
    })
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
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switchboard_payload_assembles_the_submit_shape() {
        use base64::Engine as _;
        let sw = oracle_client::SwitchboardLegsPayload {
            switchboard_package_id: "0xea".into(),
            queue_id: "0xe645d8979dac2fb901fb7c7b0ef3c9fad5dfaaf7ae2b0ce38a0b5ec63b819a99"
                .into(),
            feed_hashes: [("0x1::a::A".to_string(), "ab".repeat(32))].into(),
            quote: oracle_client::SwitchboardQuoteWire {
                feed_ids: vec!["ab".repeat(32)],
                values: vec!["63456010000000000000000".into()],
                values_neg: vec![false],
                min_oracle_samples: vec![1],
                signatures_b64: vec![
                    base64::engine::general_purpose::STANDARD.encode([7u8; 64]),
                ],
                slot: 42,
                timestamp_seconds: 1_785_700_471,
                oracle_ids: vec!["0x11".into()],
            },
        };
        let p = switchboard_payload(&sw).unwrap();
        p.validate().unwrap();
        assert_eq!(p.run_function().unwrap(), "run_1");
        assert_eq!(p.feed_ids, vec![vec![0xab; 32]]);
        assert_eq!(p.values, vec![63_456_010_000_000_000_000_000u128]);
        assert_eq!(p.oracle_ids, vec![ObjectID::from_hex_literal("0x11").unwrap()]);

        // A bad object id is a composition error, not a chain abort.
        let mut bad = sw.clone();
        bad.quote.oracle_ids = vec!["not-an-id".into()];
        assert!(switchboard_payload(&bad).is_err());
    }
}
