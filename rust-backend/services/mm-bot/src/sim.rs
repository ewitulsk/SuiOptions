//! Testnet market simulator — the desk's test counterparty (SO-299).
//!
//! The old sim rode on the DeepBook resting quoter (self-written ask
//! inventory, noise takers, spot bands); that maker died in the strategy
//! reset, so the sim's job changed with it. The spot-band DeepBook
//! liquidity simulation now lives in its own service —
//! `services/market-sim` (SO-302). This module keeps only the
//! options-side counterparty:
//!
//!   1. **Retail stand-in (writer pass)** — periodically OPENS on-chain
//!      covered-call RFQ auctions against live call buckets:
//!      faucet-mint underlying → `rfq::create_call_auction` (escrows the
//!      underlying into a generic coupled auction). The desk's
//!      `[desk.auctions]` bidder is the intended counterparty.
//!      **Implementation choice (documented)**: the on-chain rfq path was
//!      picked over selling to the desk through the WS-RFQ channel —
//!      `rfq::create_call_auction` is directly callable and the desk's
//!      auction bidder consumes it with zero extra plumbing, while the WS
//!      path would need a quoting-service *requester* client that doesn't
//!      exist in this repo.
//!   2. **Expired-position redemption** — wallet-held `Position`s redeem
//!      back to underlying + settlement after expiry so value recycles
//!      across rolls (unchanged from the old sim).
//!
//! The old noise-taker / spot-band loops are GONE from this module (their
//! config keys are silently ignored; the spot bands live on in
//! market-sim). HARD testnet gate: refuses to start unless the
//! network is testnet AND the token catalog carries faucets. Failures log
//! at `warn` — the simulator must never page anyone.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::{info, warn};

use api_service_client::{ApiServiceClient, TradeableBucket};
use protocol_types::ids::ObjectId;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::gather_exact_coin;
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

use crate::liquidity::LiquiditySource;
use crate::pricing::Staleness;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SimConfig {
    /// Master switch. Even when true the sim refuses non-testnet.
    pub enabled: bool,
    /// Auction-open / redeem pass cadence.
    pub fund_interval_secs: u64,
    /// Underlying units escrowed per opened auction.
    pub auction_amount: u64,
    /// Reserve premium as bps of the slice's spot notional. Kept LOW so
    /// the desk's discounted bid clears the floor.
    pub auction_reserve_bps: u64,
    pub auction_duration_secs: u64,
    pub auction_snipe_window_secs: u64,
    pub auction_snipe_extension_secs: u64,
    pub auction_max_extension_secs: u64,
    pub auction_min_increment_bps: u64,
    /// Stop opening new auctions while this many are already open.
    pub max_open_auctions: usize,
    pub gas_budget: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fund_interval_secs: 120,
            auction_amount: 100_000_000,
            auction_reserve_bps: 50,
            auction_duration_secs: 600,
            auction_snipe_window_secs: 60,
            auction_snipe_extension_secs: 60,
            auction_max_extension_secs: 1_800,
            auction_min_increment_bps: 100,
            max_open_auctions: 3,
            gas_budget: 100_000_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimToken {
    pub symbol: String,
    pub coin_type: String,
    pub decimals: u8,
    pub feed: Option<protocol_types::PriceFeedId>,
}

pub struct SimParams {
    pub cfg: SimConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub api_url: String,
    /// options_core package (redeem_position).
    pub core_package: ObjectID,
    /// rfq package (create_call_auction).
    pub rfq_package: Option<ObjectID>,
    pub settlement_coin_type: String,
    /// Faucet-backed source for the underlying being escrowed.
    pub liquidity: Arc<dyn LiquiditySource>,
    /// True when the token catalog carries faucets (testnet).
    pub has_faucets: bool,
    pub price_cache: pyth_client::PriceCache,
    pub staleness: Staleness,
    pub tokens: Vec<SimToken>,
}

pub fn spawn_sim(p: SimParams) {
    if !p.cfg.enabled {
        return;
    }
    if p.network != Network::Testnet {
        warn!(network = %p.network, "[sim] refusing to start: testnet only");
        return;
    }
    if !p.has_faucets {
        warn!("[sim] refusing to start: token catalog has no faucets");
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = run(&p).await {
            warn!(error = %format!("{e:#}"), "[sim] loop exited");
        }
    });
    info!("[sim] testnet counterparty armed (auction opener + redeemer)");
}

async fn run(p: &SimParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(p.api_url.clone());
    let mut kind_cache: HashMap<ObjectId, bool> = HashMap::new();
    loop {
        if let Err(e) = auction_pass(p, &wrap, &api, &mut kind_cache).await {
            warn!(error = %format!("{e:#}"), "[sim] auction pass failed; next tick");
        }
        if let Err(e) = redeem_pass(p, &wrap).await {
            warn!(error = %format!("{e:#}"), "[sim] redeem pass failed; next tick");
        }
        tokio::time::sleep(Duration::from_secs(p.cfg.fund_interval_secs)).await;
    }
}

/// call-vs-put cache: create_call_auction only exists on call buckets.
async fn is_call_bucket(
    wrap: &SuiClientWrapper,
    cache: &mut HashMap<ObjectId, bool>,
    bucket_id: &ObjectId,
) -> bool {
    if let Some(v) = cache.get(bucket_id) {
        return *v;
    }
    let Ok(oid) = ObjectID::from_hex_literal(&bucket_id.to_hex()) else { return false };
    let is_call = wrap
        .client
        .get_object(oid)
        .await
        .ok()
        .and_then(|o| o.struct_tag())
        .map(|t| {
            t.to_canonical_string(/* with_prefix */ true)
                .contains("::bucket::Bucket<")
        })
        .unwrap_or(false);
    cache.insert(bucket_id.clone(), is_call);
    is_call
}

/// Open one covered-call auction per pass (retail drip, not a firehose).
async fn auction_pass(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    kind_cache: &mut HashMap<ObjectId, bool>,
) -> Result<()> {
    let Some(rfq_package) = p.rfq_package else {
        warn!("[sim] no rfq package in token-info; auction opener idle");
        return Ok(());
    };
    let open = api.open_rfqs().await.unwrap_or_default();
    if open.len() >= p.cfg.max_open_auctions {
        return Ok(());
    }
    let now = now_ms();
    // The auction must fully fit (with max extension) before expiry.
    let horizon_ms = (p.cfg.auction_duration_secs + p.cfg.auction_max_extension_secs + 60) * 1_000;
    let buckets = api.tradeable_buckets().await?;
    for b in &buckets {
        if b.invalidated || b.expiry_ms <= now.saturating_add(horizon_ms) {
            continue;
        }
        if !is_call_bucket(wrap, kind_cache, &b.bucket_id).await {
            continue;
        }
        // Skip buckets that already have an open auction.
        if open.iter().any(|r| r.bucket_id == b.bucket_id) {
            continue;
        }
        let amount = p.cfg.auction_amount;
        let have = p
            .liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &b.asset_coin_type, amount)
            .await;
        if have < amount {
            warn!(bucket = %b.bucket_id, "[sim] underlying faucet came up short; skipping");
            continue;
        }
        let reserve = reserve_premium(p, b, amount);
        match create_call_auction(p, wrap, rfq_package, b, amount, reserve).await {
            Ok(()) => {
                info!(
                    bucket = %b.bucket_id,
                    amount,
                    reserve,
                    "[sim] opened covered-call RFQ auction (retail stand-in)"
                );
                return Ok(()); // one per pass
            }
            Err(e) => {
                warn!(bucket = %b.bucket_id, error = %format!("{e:#}"), "[sim] auction open failed");
            }
        }
    }
    Ok(())
}

/// Reserve premium: `auction_reserve_bps` of the slice's spot notional
/// (floor 1). Falls back to strike notional when the feed is stale — a
/// too-high reserve just means an unsold slice, never a mispriced desk.
fn reserve_premium(p: &SimParams, b: &TradeableBucket, amount: u64) -> u64 {
    let spot = p
        .tokens
        .iter()
        .find(|t| {
            protocol_types::asset::canonicalize_move_type(&t.coin_type)
                == protocol_types::asset::canonicalize_move_type(&b.asset_coin_type)
        })
        .and_then(|t| {
            let feed = t.feed?;
            let settle = p.tokens.iter().find(|s| {
                protocol_types::asset::canonicalize_move_type(&s.coin_type)
                    == protocol_types::asset::canonicalize_move_type(&b.settlement_coin_type)
            })?;
            crate::pricing::compute_spot_from_cache(
                &p.price_cache,
                feed,
                settle.feed?,
                t.decimals,
                settle.decimals,
                p.staleness,
            )
            .ok()
        });
    let notional = match spot {
        Some(s) => s * amount as f64,
        None => {
            let scale = 10f64.powi(b.strike_scale as i32);
            (b.strike_raw as f64 / scale) * amount as f64
        }
    };
    ((notional * p.cfg.auction_reserve_bps as f64 / 10_000.0) as u64).max(1)
}

/// `rfq::create_call_auction`: escrow faucet-minted underlying into a
/// coupled auction. Position + proceeds recipients are the sim wallet
/// (the redeem pass exits the Position after expiry).
async fn create_call_auction(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    rfq_package: ObjectID,
    b: &TradeableBucket,
    amount: u64,
    reserve_premium: u64,
) -> Result<()> {
    let bucket_oid = ObjectID::from_hex_literal(&b.bucket_id.to_hex())?;
    let tags = vec![
        TypeTag::from_str(&b.asset_coin_type)?,
        TypeTag::from_str(&b.settlement_coin_type)?,
        TypeTag::from_str(&b.call_coin_type)?,
    ];
    let mut pt = ProgrammableTransactionBuilder::new();
    let underlying =
        gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &b.asset_coin_type, amount).await?;
    let bucket = pt.obj(shared_object_arg(&wrap.client, bucket_oid, false).await?)?;
    let a_reserve = pt.pure(reserve_premium)?;
    let a_duration = pt.pure(p.cfg.auction_duration_secs * 1_000)?;
    let a_snipe = pt.pure(p.cfg.auction_snipe_window_secs * 1_000)?;
    let a_snipe_ext = pt.pure(p.cfg.auction_snipe_extension_secs * 1_000)?;
    let a_max_ext = pt.pure(p.cfg.auction_max_extension_secs * 1_000)?;
    let a_incr = pt.pure(p.cfg.auction_min_increment_bps)?;
    let a_pos_rcpt = pt.pure(wrap.signer.address)?;
    let a_proc_rcpt = pt.pure(wrap.signer.address)?;
    // Seller-origin attribution: our address as an ID.
    let a_origin = pt.pure(ObjectID::new(wrap.signer.address.to_inner()))?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        rfq_package,
        Identifier::new("rfq").unwrap(),
        Identifier::new("create_call_auction").unwrap(),
        tags,
        vec![
            bucket, underlying, a_reserve, a_duration, a_snipe, a_snipe_ext, a_max_ext, a_incr,
            a_pos_rcpt, a_proc_rcpt, a_origin, clock,
        ],
    );
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::create_call_auction")
        .await?;
    Ok(())
}

/// Redeem wallet-held Positions whose buckets have expired (unchanged
/// from the old sim).
async fn redeem_pass(p: &SimParams, wrap: &SuiClientWrapper) -> Result<()> {
    let position_type = format!("{}::position::Position", p.core_package);
    let positions = wrap
        .client
        .owned_objects_of_type(
            wrap.signer.address,
            sui_types::parse_sui_struct_tag(&position_type)?,
            25,
        )
        .await?;
    let now = now_ms();
    for obj in positions {
        // The owned-object listing carries BCS only; one read per position
        // gets the JSON rendering the bucket link is read from.
        let Ok((_, Some(json))) = wrap.client.get_object_json(obj.id()).await else {
            continue;
        };
        let Some(bucket_hex) = json.pointer("/bucket_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(bucket_oid) = ObjectID::from_hex_literal(bucket_hex) else { continue };
        // Only call buckets: puts are never self-written by the sim.
        let Ok((bucket_obj, bucket_json)) = wrap.client.get_object_json(bucket_oid).await else {
            continue;
        };
        let ty = bucket_obj
            .struct_tag()
            .map(|t| t.to_canonical_string(/* with_prefix */ true))
            .unwrap_or_default();
        if !ty.contains("::bucket::Bucket<") {
            continue;
        }
        let expiry = bucket_json
            .and_then(|j| {
                j.pointer("/expiry_ms")
                    .and_then(|v| v.as_str().map(String::from))
            })
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if expiry > now {
            continue;
        }
        let inner = ty
            .split_once('<')
            .and_then(|(_, r)| r.strip_suffix('>'))
            .unwrap_or_default();
        let parts: Vec<&str> = split_top(inner);
        if parts.len() != 3 {
            continue;
        }
        let tags: Vec<TypeTag> = match parts.iter().map(|s| TypeTag::from_str(s)).collect() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mut pt = ProgrammableTransactionBuilder::new();
        let bucket_arg = pt.obj(shared_object_arg(&wrap.client, bucket_oid, true).await?)?;
        let pos = pt.obj(sui_tx::tx::owned_object_arg(&wrap.client, obj.id()).await?)?;
        let clock = clock_arg(&mut pt)?;
        let out = pt.programmable_move_call(
            p.core_package,
            Identifier::new("bucket").unwrap(),
            Identifier::new("redeem_position").unwrap(),
            tags,
            vec![bucket_arg, pos, clock],
        );
        let sender = pt.pure(wrap.signer.address)?;
        pt.command(sui_types::transaction::Command::TransferObjects(
            vec![
                sui_types::transaction::Argument::NestedResult(nested_of(out), 0),
                sui_types::transaction::Argument::NestedResult(nested_of(out), 1),
            ],
            sender,
        ));
        match submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::redeem").await {
            Ok(_) => info!(position = %obj.id(), "[sim] redeemed expired position"),
            Err(e) => warn!(position = %obj.id(), error = %format!("{e:#}"), "[sim] redeem failed"),
        }
    }
    Ok(())
}

fn nested_of(arg: sui_types::transaction::Argument) -> u16 {
    match arg {
        sui_types::transaction::Argument::Result(i) => i,
        _ => 0,
    }
}

fn split_top(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < inner.len() {
        out.push(inner[start..].trim());
    }
    out
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
