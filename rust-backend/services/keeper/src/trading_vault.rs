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
//!   6. Fulfill the withdrawal queue with a FULL attestation-bearing
//!      appraisal (sui_tx::tx::appraisal composer) — cash-only vaults
//!      need no price legs, everything else gets Pyth attestations.
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
    compose_appraisal, discover_holdings, pyth_assets_needed, AppraisalRefs, OptionBucketInfo,
    PositionInfo, PriceLegs, VaultHoldings,
};
use sui_tx::tx::pyth_update::PythHandles;
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

use crate::discovery::{price_info_object_for, resolve_price_info_table, PriceInfoTable};

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
    for v in vaults {
        if v.state == "closed" && v.pending_withdrawals == 0 {
            continue;
        }
        let vault_id = match ObjectID::from_hex_literal(&v.vault_id.to_hex()) {
            Ok(id) => id,
            Err(_) => continue,
        };
        if let Err(e) =
            tick_one(wrap, http, ctx, vault_id, v.pending_withdrawals, &option_buckets).await
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
    if benign.iter().any(|b| msg.contains(b)) {
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

async fn tick_one(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    pending_withdrawals: u64,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<()> {
    let client = &wrap.client;
    let holdings = discover_holdings(client, vault_id).await?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    settle_due_tickets(wrap, ctx, vault_id, &holdings, now_ms).await;
    redeem_expired_positions(wrap, ctx, vault_id, &holdings, now_ms).await;
    sweep_custody_settled(wrap, ctx, vault_id, &holdings).await;
    sweep_vault_address(wrap, ctx, vault_id).await;
    force_unwind_if_starved(wrap, ctx, vault_id, &holdings, now_ms).await;

    if pending_withdrawals > 0 {
        // Re-discover: the cranks above may have changed holdings.
        let holdings = discover_holdings(client, vault_id).await?;
        fulfill(wrap, http, ctx, vault_id, &holdings, option_buckets).await?;
    }
    Ok(())
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
    }
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

/// Crank 6: fulfillment with a full appraisal.
async fn fulfill(
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    ctx: &TradingVaultCtx,
    vault_id: ObjectID,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<()> {
    let client = &wrap.client;
    let refs = refs_for(ctx, vault_id);
    let mut pt = ProgrammableTransactionBuilder::new();

    // Option-coin types price via the options oracle; only the remaining
    // (underlying/settlement/plain) types need pyth feeds.
    let needed = pyth_assets_needed(holdings, option_buckets);
    let appraisal = if needed.is_empty() {
        compose_appraisal(client, &mut pt, &refs, holdings, None, option_buckets).await?
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
            &mut pt,
            &refs,
            holdings,
            Some(PriceLegs { pyth: &ctx.pyth, accumulator_update: update, price_infos: &price_infos }),
            option_buckets,
        )
        .await?
    };

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
) -> Result<Option<TradingVaultCtx>> {
    let Some(tv) = snapshot.trading_vault() else { return Ok(None) };
    let Some(op) = snapshot.oracle_pyth() else { return Ok(None) };
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
    let price_table = resolve_price_info_table(client, pyth.pyth_state_id).await.ok();
    if price_table.is_none() {
        warn!("pyth price_info table unresolved; multi-asset fulfillment disabled");
    }

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
    }))
}
