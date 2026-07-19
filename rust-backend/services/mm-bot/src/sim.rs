//! Testnet market simulator (SO-296): the layers that turn the DeepBook
//! quoter into a self-sustaining fake market so the trading-vault
//! product can be exercised like a real venue.
//!
//! Three loops around the EXISTING quoter (which stays the maker):
//!
//!   1. **Ask-inventory writer** — the quoter's faucet `LiquiditySource`
//!      already keeps the settlement (bid) side funded, but asks need
//!      call coins, which cannot be faucet-minted. The writer pass
//!      self-writes them: faucet-mint the underlying →
//!      `bucket::write_collateralized` → the `Coin<CALL>` lands in the
//!      wallet (the quoter's inventory sweep lists it), the `Position`
//!      stays in the wallet for post-expiry redemption. This also keeps
//!      the sim economically honest — its asks are genuinely covered.
//!   2. **Position redemption** — after expiry, wallet-held Positions
//!      are redeemed back to underlying + settlement so value recycles
//!      across rolls instead of accumulating dead objects.
//!   3. **Noise takers** — resting liquidity alone never fills anyone.
//!      A low-rate taker loop crosses the spread from a SECOND
//!      BalanceManager (different owner-side than the maker BM is not
//!      needed — a different BM id suffices for DeepBook matching):
//!      market-buys with faucet-minted settlement, market-sells the
//!      base inventory those buys accumulate. This is what gives vault
//!      curators fills, inventory drift, and a moving NAV.
//!
//! HARD testnet gate: refuses to start unless the network is testnet
//! AND the token catalog carries faucets. Failures log at `warn` — the
//! simulator must never page anyone.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde::Deserialize;
use sui_sdk::rpc_types::SuiObjectDataOptions;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::{info, warn};

use api_service_client::{ApiServiceClient, TradeableBucket};
use protocol_types::ids::ObjectId;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{
    bm_balance, create_balance_manager, gather_exact_coin, DeepBookHandles,
};
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

use crate::liquidity::LiquiditySource;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SimConfig {
    /// Master switch. Even when true the sim refuses non-testnet.
    pub enabled: bool,
    /// Writer/redeem pass cadence.
    pub fund_interval_secs: u64,
    /// Self-write chunk per underfunded bucket, as a multiple of the
    /// quoter's `quote_size` (target inventory = the same amount).
    pub ask_refill_multiple: u64,
    /// Noise-taker switch + cadence (uniform jitter on top).
    pub taker_enabled: bool,
    pub taker_interval_secs: u64,
    pub taker_jitter_secs: u64,
    /// Taker order size in pool lots.
    pub taker_size_lots: u64,
    /// Rolling notional spend cap (settlement atomic units per hour).
    pub taker_max_notional_per_hour: u64,
    /// The taker's own BalanceManager; empty = create at boot and log.
    pub taker_balance_manager_id: Option<String>,
    /// Spot pairs to band, as "BASE/QUOTE" symbols (e.g. "TSUI/TUSDC").
    /// Pools are created LAZILY on first liquidity deployment: looked up
    /// by PoolCreated event, created via create_permissionless_pool when
    /// missing (costs `pool_creation_fee` vendored DEEP from the bot
    /// wallet — fund it or the pair is skipped with a loud warning).
    pub spot_pairs: Vec<String>,
    pub spot_interval_secs: u64,
    /// Half-band around the Pyth cross, bps.
    pub spot_band_bps: u64,
    /// Per-side size as settlement notional (atomic units).
    pub spot_notional_per_side: u64,
    /// The spot maker's own BalanceManager; empty = create at boot + log.
    pub spot_balance_manager_id: Option<String>,
    pub gas_budget: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fund_interval_secs: 60,
            ask_refill_multiple: 4,
            taker_enabled: true,
            taker_interval_secs: 45,
            taker_jitter_secs: 30,
            taker_size_lots: 1,
            taker_max_notional_per_hour: 5_000_000_000,
            taker_balance_manager_id: None,
            spot_pairs: Vec::new(),
            spot_interval_secs: 60,
            spot_band_bps: 200,
            spot_notional_per_side: 100_000_000,
            spot_balance_manager_id: None,
            gas_budget: 100_000_000,
        }
    }
}

pub struct SimParams {
    pub cfg: SimConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub handles: DeepBookHandles,
    pub api_url: String,
    /// options_core package (write_collateralized / redeem_position).
    pub core_package: ObjectID,
    pub settlement_coin_type: String,
    /// The quoter's per-side size — the writer pass refills toward
    /// `quote_size × ask_refill_multiple`.
    pub quote_size: u64,
    /// Faucet-backed source (same instance the quoter uses).
    pub liquidity: Arc<dyn LiquiditySource>,
    /// True when the token catalog carries faucets (testnet).
    pub has_faucets: bool,
    /// Pyth cache + staleness bounds (shared with the quoter) and the
    /// token catalog — the spot loop prices bands straight off these.
    pub price_cache: pyth_client::PriceCache,
    pub staleness: crate::pricing::Staleness,
    pub tokens: Vec<SimToken>,
    /// Vendored-DEEP creation fee context for lazy spot pools.
    pub deep_coin_type: String,
    pub pool_creation_fee: u64,
}

#[derive(Debug, Clone)]
pub struct SimToken {
    pub symbol: String,
    pub coin_type: String,
    pub decimals: u8,
    pub feed: Option<protocol_types::PriceFeedId>,
}

/// A lazily-ensured spot pool, shared with the taker loop.
#[derive(Debug, Clone)]
pub struct SpotPool {
    pub pool_id: ObjectID,
    pub base: SimToken,
    pub quote: SimToken,
}

/// Dependency-free xorshift; statistical quality is irrelevant here.
struct Rng(u64);
impl Rng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15);
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
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
    let p = Arc::new(p);
    let spot_pools: Arc<tokio::sync::Mutex<Vec<SpotPool>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    {
        let p = Arc::clone(&p);
        tokio::spawn(async move {
            if let Err(e) = writer_loop(&p).await {
                warn!(error = %format!("{e:#}"), "[sim] writer loop exited");
            }
        });
    }
    if !p.cfg.spot_pairs.is_empty() {
        let p = Arc::clone(&p);
        let pools = Arc::clone(&spot_pools);
        tokio::spawn(async move {
            if let Err(e) = spot_loop(&p, pools).await {
                warn!(error = %format!("{e:#}"), "[sim] spot loop exited");
            }
        });
    }
    if p.cfg.taker_enabled {
        let p = Arc::clone(&p);
        let pools = Arc::clone(&spot_pools);
        tokio::spawn(async move {
            if let Err(e) = taker_loop(&p, pools).await {
                warn!(error = %format!("{e:#}"), "[sim] taker loop exited");
            }
        });
    }
    info!("[sim] testnet market simulator armed (writer + spot + takers)");
}

/// call-vs-put cache: write_collateralized only exists on call buckets.
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
        .read_api()
        .get_object_with_options(oid, SuiObjectDataOptions::new().with_type())
        .await
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.type_)
        .map(|t| t.to_string().contains("::bucket::Bucket<"))
        .unwrap_or(false);
    cache.insert(bucket_id.clone(), is_call);
    is_call
}

async fn writer_loop(p: &SimParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(p.api_url.clone());
    let mut kind_cache: HashMap<ObjectId, bool> = HashMap::new();
    loop {
        if let Err(e) = writer_pass(p, &wrap, &api, &mut kind_cache).await {
            warn!(error = %format!("{e:#}"), "[sim] writer pass failed; next tick");
        }
        if let Err(e) = redeem_pass(p, &wrap).await {
            warn!(error = %format!("{e:#}"), "[sim] redeem pass failed; next tick");
        }
        tokio::time::sleep(Duration::from_secs(p.cfg.fund_interval_secs)).await;
    }
}

/// Top up call-coin ask inventory for every live call bucket.
async fn writer_pass(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    kind_cache: &mut HashMap<ObjectId, bool>,
) -> Result<()> {
    let target = p.quote_size.saturating_mul(p.cfg.ask_refill_multiple.max(1));
    let now = now_ms();
    let buckets = api.tradeable_buckets().await?;
    for b in &buckets {
        if b.invalidated || b.expiry_ms <= now || b.pool_id.is_empty() {
            continue;
        }
        if !is_call_bucket(wrap, kind_cache, &b.bucket_id).await {
            continue;
        }
        // Inventory anywhere the quoter can list it: wallet + wherever
        // it already rests is approximated by wallet holdings — the
        // quoter sweeps the wallet, so a low wallet balance with resting
        // asks just writes a little extra. Cheap and self-correcting.
        let held = wallet_balance(wrap, &b.call_coin_type).await;
        if held >= target {
            continue;
        }
        let write_amount = target - held;
        let have = p
            .liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &b.asset_coin_type, write_amount)
            .await;
        if have < write_amount {
            warn!(bucket = %b.bucket_id, "[sim] underlying faucet came up short; skipping");
            continue;
        }
        if let Err(e) = self_write(p, wrap, b, write_amount).await {
            warn!(bucket = %b.bucket_id, error = %format!("{e:#}"), "[sim] self-write failed");
        } else {
            info!(bucket = %b.bucket_id, write_amount, "[sim] wrote call inventory");
        }
    }
    Ok(())
}

/// mint-backed `write_collateralized`: underlying in, Position +
/// Coin<CALL> back to the wallet.
async fn self_write(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    b: &TradeableBucket,
    amount: u64,
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
    let bucket = pt.obj(shared_object_arg(&wrap.client, bucket_oid, true).await?)?;
    let clock = clock_arg(&mut pt)?;
    let result = pt.programmable_move_call(
        p.core_package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("write_collateralized").unwrap(),
        tags,
        vec![bucket, underlying, clock],
    );
    let sender = pt.pure(wrap.signer.address)?;
    pt.command(sui_types::transaction::Command::TransferObjects(
        vec![
            sui_types::transaction::Argument::NestedResult(nested_of(result), 0),
            sui_types::transaction::Argument::NestedResult(nested_of(result), 1),
        ],
        sender,
    ));
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::self_write").await?;
    Ok(())
}

fn nested_of(arg: sui_types::transaction::Argument) -> u16 {
    match arg {
        sui_types::transaction::Argument::Result(i) => i,
        _ => 0,
    }
}

/// Redeem wallet-held Positions whose buckets have expired.
async fn redeem_pass(p: &SimParams, wrap: &SuiClientWrapper) -> Result<()> {
    let position_type = format!("{}::position::Position", p.core_package);
    let filter = sui_sdk::rpc_types::SuiObjectResponseQuery::new(
        Some(sui_sdk::rpc_types::SuiObjectDataFilter::StructType(
            sui_types::parse_sui_struct_tag(&position_type)?,
        )),
        Some(SuiObjectDataOptions::new().with_content().with_type()),
    );
    let page = wrap
        .client
        .read_api()
        .get_owned_objects(wrap.signer.address, Some(filter), None, Some(25))
        .await?;
    let now = now_ms();
    for obj in page.data {
        let Some(d) = obj.data else { continue };
        let Some(content) = d.content else { continue };
        let json = serde_json::to_value(&content).unwrap_or_default();
        let Some(bucket_hex) = json.pointer("/fields/bucket_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(bucket_oid) = ObjectID::from_hex_literal(bucket_hex) else { continue };
        // Only call buckets: puts are never self-written by the sim.
        let bucket = wrap
            .client
            .read_api()
            .get_object_with_options(
                bucket_oid,
                SuiObjectDataOptions::new().with_type().with_content(),
            )
            .await
            .ok()
            .and_then(|r| r.data);
        let Some(bd) = bucket else { continue };
        let ty = bd.type_.map(|t| t.to_string()).unwrap_or_default();
        if !ty.contains("::bucket::Bucket<") {
            continue;
        }
        let expiry = bd
            .content
            .and_then(|c| serde_json::to_value(&c).ok())
            .and_then(|j| {
                j.pointer("/fields/expiry_ms")
                    .and_then(|v| v.as_str().map(String::from))
            })
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if expiry > now {
            continue;
        }
        let inner = ty
            .split_once('<')
            .map(|(_, r)| r.trim_end_matches('>'))
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
        let pos = pt.obj(sui_tx::tx::owned_object_arg(&wrap.client, d.object_id).await?)?;
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
            Ok(_) => info!(position = %d.object_id, "[sim] redeemed expired position"),
            Err(e) => warn!(position = %d.object_id, error = %format!("{e:#}"), "[sim] redeem failed"),
        }
    }
    Ok(())
}


/// Lazily ensure + band the configured spot pairs. Pools are created on
/// the FIRST liquidity deployment attempt: looked up by their
/// `PoolCreated<Base, Quote>` event, created when absent (vendored-DEEP
/// fee from the bot wallet), then quoted around the Pyth cross.
async fn spot_loop(p: &SimParams, shared: Arc<tokio::sync::Mutex<Vec<SpotPool>>>) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let bm_id = match p.cfg.spot_balance_manager_id.as_deref() {
        Some(id) if !id.is_empty() => ObjectID::from_hex_literal(id)?,
        _ => {
            let mut created = None;
            for attempt in 1..=8u32 {
                match create_balance_manager(&wrap.client, &wrap.signer, &p.handles, p.cfg.gas_budget)
                    .await
                {
                    Ok(id) => {
                        info!(bm = %id, "[sim] created spot BalanceManager — persist as [sim].spot_balance_manager_id");
                        created = Some(id);
                        break;
                    }
                    Err(e) => {
                        warn!(attempt, error = %format!("{e:#}"), "[sim] spot BM creation failed; retrying");
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
            created.ok_or_else(|| anyhow!("spot BalanceManager creation kept failing"))?
        }
    };

    // Resolve configured pairs against the token catalog once.
    let mut pairs: Vec<(SimToken, SimToken)> = Vec::new();
    for pair in &p.cfg.spot_pairs {
        let Some((b, q)) = pair.split_once('/') else {
            warn!(pair, "[sim] bad spot pair (want BASE/QUOTE symbols)");
            continue;
        };
        let find = |sym: &str| p.tokens.iter().find(|t| t.symbol.eq_ignore_ascii_case(sym)).cloned();
        match (find(b), find(q)) {
            (Some(base), Some(quote)) if base.feed.is_some() && quote.feed.is_some() => {
                pairs.push((base, quote))
            }
            _ => warn!(pair, "[sim] spot pair tokens missing from catalog (or no feed); skipped"),
        }
    }

    loop {
        for (base, quote) in &pairs {
            // Already ensured?
            let known = {
                let pools = shared.lock().await;
                pools
                    .iter()
                    .find(|sp| sp.base.coin_type == base.coin_type && sp.quote.coin_type == quote.coin_type)
                    .map(|sp| sp.pool_id)
            };
            let pool_id = match known {
                Some(id) => id,
                None => match ensure_spot_pool(p, &wrap, base, quote).await {
                    Ok(Some(id)) => {
                        shared.lock().await.push(SpotPool {
                            pool_id: id,
                            base: base.clone(),
                            quote: quote.clone(),
                        });
                        id
                    }
                    Ok(None) => continue, // unfunded; retried next pass
                    Err(e) => {
                        warn!(base = %base.symbol, quote = %quote.symbol, error = %format!("{e:#}"), "[sim] spot pool ensure failed");
                        continue;
                    }
                },
            };
            if let Err(e) = spot_quote_pass(p, &wrap, bm_id, pool_id, base, quote).await {
                warn!(pool = %pool_id, error = %format!("{e:#}"), "[sim] spot quote pass failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(p.cfg.spot_interval_secs)).await;
    }
}

/// Find the pair's pool by its creation event, else create it. `None` =
/// wallet lacks the vendored-DEEP fee (warned; retried next pass).
async fn ensure_spot_pool(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    base: &SimToken,
    quote: &SimToken,
) -> Result<Option<ObjectID>> {
    use sui_sdk::rpc_types::EventFilter;
    let event_type = format!(
        "{}::pool::PoolCreated<{}, {}>",
        p.handles.original_package, base.coin_type, quote.coin_type
    );
    if let Ok(tag) = sui_types::parse_sui_struct_tag(&event_type) {
        if let Ok(page) = wrap
            .client
            .event_api()
            .query_events(EventFilter::MoveEventType(tag), None, Some(1), true)
            .await
        {
            if let Some(ev) = page.data.first() {
                if let Some(id) = ev
                    .parsed_json
                    .pointer("/pool_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                {
                    info!(pool = %id, base = %base.symbol, quote = %quote.symbol, "[sim] found existing spot pool");
                    return Ok(Some(id));
                }
            }
        }
    }
    // No pool: create it — needs the vendored-DEEP creation fee.
    let deep = wallet_balance(wrap, &p.deep_coin_type).await;
    if deep < p.pool_creation_fee {
        warn!(
            base = %base.symbol,
            quote = %quote.symbol,
            need = p.pool_creation_fee,
            have = deep,
            wallet = %wrap.signer.address,
            "[sim] cannot create spot pool: wallet lacks vendored DEEP — \
             transfer the fee from the deployer wallet to enable this pair"
        );
        return Ok(None);
    }
    let id = sui_tx::tx::deepbook::create_pool(
        &wrap.client,
        &wrap.signer,
        &p.handles,
        &p.deep_coin_type,
        p.pool_creation_fee,
        &base.coin_type,
        &quote.coin_type,
        base.decimals,
        quote.decimals,
        p.cfg.gas_budget,
    )
    .await?;
    info!(
        pool = %id,
        base = %base.symbol,
        quote = %quote.symbol,
        "[sim] created spot pool lazily — have an admin run deepbook_adapter::allow_pool to vet it for vault curators"
    );
    Ok(Some(id))
}

/// One banding pass: fund both sides from the faucet, cancel, re-quote
/// bid/ask around the Pyth cross.
async fn spot_quote_pass(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    bm_id: ObjectID,
    pool_id: ObjectID,
    base: &SimToken,
    quote: &SimToken,
) -> Result<()> {
    let mid = crate::pricing::compute_spot_from_cache(
        &p.price_cache,
        base.feed.ok_or_else(|| anyhow!("no base feed"))?,
        quote.feed.ok_or_else(|| anyhow!("no quote feed"))?,
        base.decimals,
        quote.decimals,
        p.staleness,
    )
    .map_err(|e| anyhow!("stale spot for {}/{}: {e:?}", base.symbol, quote.symbol))?;

    let (tick, lot, min) = sui_tx::tx::deepbook::derived_pool_params(base.decimals, quote.decimals);
    let round_tick = |px: f64| -> u64 {
        let raw = (px * 1e9) as u64;
        ((raw / tick).max(1)) * tick
    };
    let band = p.cfg.spot_band_bps as f64 / 10_000.0;
    let bid_px = round_tick(mid * (1.0 - band));
    let ask_px = round_tick(mid * (1.0 + band));
    let qty = {
        let base_units = (p.cfg.spot_notional_per_side as f64 / mid) as u64;
        ((base_units / lot).max(1)) * lot
    }
    .max(min);

    // Fund both sides: quote notional for the bid, base qty for the ask.
    let quote_need = ((qty as u128 * ask_px as u128) / 1_000_000_000) as u64;
    let have_q = p
        .liquidity
        .ensure_wallet_balance(&wrap.client, &wrap.signer, &quote.coin_type, quote_need)
        .await;
    let have_b = p
        .liquidity
        .ensure_wallet_balance(&wrap.client, &wrap.signer, &base.coin_type, qty)
        .await;
    if have_q < quote_need || have_b < qty {
        return Err(anyhow!("faucet came up short for {}/{}", base.symbol, quote.symbol));
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
    let qcoin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &quote.coin_type, quote_need).await?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![TypeTag::from_str(&quote.coin_type)?],
        vec![bm, qcoin],
    );
    let bcoin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &base.coin_type, qty).await?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![TypeTag::from_str(&base.coin_type)?],
        vec![bm, bcoin],
    );

    // Cancel + requote in the same PTB.
    let pool = pt.obj(shared_object_arg(&wrap.client, pool_id, true).await?)?;
    let proof = pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );
    let tags = vec![TypeTag::from_str(&base.coin_type)?, TypeTag::from_str(&quote.coin_type)?];
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("cancel_all_orders").unwrap(),
        tags.clone(),
        vec![pool, bm, proof, clock],
    );
    let expire = now_ms() + 10 * 60 * 1000;
    for (px, is_bid) in [(bid_px, true), (ask_px, false)] {
        let a_client = pt.pure(now_ms() / 60_000)?;
        let a_type = pt.pure(3u8)?; // post-only
        let a_self = pt.pure(0u8)?;
        let a_px = pt.pure(px)?;
        let a_qty = pt.pure(qty)?;
        let a_bid = pt.pure(is_bid)?;
        let a_deep = pt.pure(false)?;
        let a_exp = pt.pure(expire)?;
        pt.programmable_move_call(
            p.handles.package,
            Identifier::new("pool").unwrap(),
            Identifier::new("place_limit_order").unwrap(),
            tags.clone(),
            vec![pool, bm, proof, a_client, a_type, a_self, a_px, a_qty, a_bid, a_deep, a_exp, clock],
        );
    }
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("withdraw_settled_amounts_permissionless").unwrap(),
        tags,
        vec![pool, bm],
    );
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::spot_quote").await?;
    info!(pool = %pool_id, base = %base.symbol, bid_px, ask_px, qty, "[sim] spot band refreshed");
    Ok(())
}

async fn taker_loop(p: &SimParams, spot_pools: Arc<tokio::sync::Mutex<Vec<SpotPool>>>) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(p.api_url.clone());
    let mut rng = Rng::new();

    // The taker's own BM (never the maker's): configured or created —
    // with retries, because boot-time gas-coin races with the other
    // services sharing this wallet are routine.
    let bm_id = match p.cfg.taker_balance_manager_id.as_deref() {
        Some(id) if !id.is_empty() => ObjectID::from_hex_literal(id)?,
        _ => {
            let mut created = None;
            for attempt in 1..=8u32 {
                match create_balance_manager(&wrap.client, &wrap.signer, &p.handles, p.cfg.gas_budget)
                    .await
                {
                    Ok(id) => {
                        info!(bm = %id, "[sim] created taker BalanceManager — persist as [sim].taker_balance_manager_id");
                        created = Some(id);
                        break;
                    }
                    Err(e) => {
                        warn!(attempt, error = %format!("{e:#}"), "[sim] taker BM creation failed; retrying");
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
            created.ok_or_else(|| anyhow!("taker BalanceManager creation kept failing"))?
        }
    };

    // Rolling hourly notional budget.
    let mut window_start = now_ms();
    let mut spent: u64 = 0;
    let mut kind_cache: HashMap<ObjectId, bool> = HashMap::new();

    loop {
        let jitter = rng.below(p.cfg.taker_jitter_secs.max(1));
        tokio::time::sleep(Duration::from_secs(p.cfg.taker_interval_secs + jitter)).await;

        let now = now_ms();
        if now.saturating_sub(window_start) > 3_600_000 {
            window_start = now;
            spent = 0;
        }
        if spent >= p.cfg.taker_max_notional_per_hour {
            continue;
        }
        let spot_choice = {
            let pools = spot_pools.lock().await;
            if pools.is_empty() { None } else { Some(pools[rng.below(pools.len() as u64) as usize].clone()) }
        };
        let use_spot = spot_choice.is_some() && rng.below(10) < 4;
        let r = if use_spot {
            spot_taker_tick(p, &wrap, bm_id, spot_choice.as_ref().unwrap(), &mut rng, &mut spent).await
        } else {
            taker_tick(p, &wrap, &api, bm_id, &mut rng, &mut spent, &mut kind_cache).await
        };
        if let Err(e) = r {
            warn!(error = %format!("{e:#}"), "[sim] taker tick failed");
        }
    }
}

/// Cross a spot band: buy with faucet quote, or sell faucet-minted base.
async fn spot_taker_tick(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    bm_id: ObjectID,
    sp: &SpotPool,
    rng: &mut Rng,
    spent: &mut u64,
) -> Result<()> {
    let (_, lot, min) = sui_tx::tx::deepbook::derived_pool_params(sp.base.decimals, sp.quote.decimals);
    let qty = (lot.saturating_mul(p.cfg.taker_size_lots.max(1))).max(min);
    let is_bid = rng.below(2) == 0;

    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
    if is_bid {
        // Budget generously off the Pyth cross (2x mid notional).
        let mid = crate::pricing::compute_spot_from_cache(
            &p.price_cache,
            sp.base.feed.ok_or_else(|| anyhow!("no base feed"))?,
            sp.quote.feed.ok_or_else(|| anyhow!("no quote feed"))?,
            sp.base.decimals,
            sp.quote.decimals,
            p.staleness,
        )
        .map_err(|e| anyhow!("stale spot: {e:?}"))?;
        let budget = ((qty as f64 * mid * 2.0) as u64).max(1_000_000);
        let have = p
            .liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &sp.quote.coin_type, budget)
            .await;
        if have < budget {
            return Ok(());
        }
        let coin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &sp.quote.coin_type, budget).await?;
        pt.programmable_move_call(
            p.handles.package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![TypeTag::from_str(&sp.quote.coin_type)?],
            vec![bm, coin],
        );
        *spent = spent.saturating_add(budget);
    } else {
        let have = p
            .liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &sp.base.coin_type, qty)
            .await;
        if have < qty {
            return Ok(());
        }
        let coin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &sp.base.coin_type, qty).await?;
        pt.programmable_move_call(
            p.handles.package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![TypeTag::from_str(&sp.base.coin_type)?],
            vec![bm, coin],
        );
    }
    let pool = pt.obj(shared_object_arg(&wrap.client, sp.pool_id, true).await?)?;
    let proof = pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );
    let tags = vec![
        TypeTag::from_str(&sp.base.coin_type)?,
        TypeTag::from_str(&sp.quote.coin_type)?,
    ];
    let clock = clock_arg(&mut pt)?;
    let a_client = pt.pure(now_ms() / 60_000)?;
    let a_self = pt.pure(0u8)?;
    let a_qty = pt.pure(qty)?;
    let a_bid = pt.pure(is_bid)?;
    let a_deep = pt.pure(false)?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("place_market_order").unwrap(),
        tags.clone(),
        vec![pool, bm, proof, a_client, a_self, a_qty, a_bid, a_deep, clock],
    );
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("withdraw_settled_amounts_permissionless").unwrap(),
        tags,
        vec![pool, bm],
    );
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::spot_taker").await?;
    info!(pool = %sp.pool_id, qty, is_bid, "[sim] spot taker crossed");
    Ok(())
}

async fn taker_tick(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    bm_id: ObjectID,
    rng: &mut Rng,
    spent: &mut u64,
    kind_cache: &mut HashMap<ObjectId, bool>,
) -> Result<()> {
    let now = now_ms();
    let buckets: Vec<TradeableBucket> = api
        .tradeable_buckets()
        .await?
        .into_iter()
        .filter(|b| !b.invalidated && b.expiry_ms > now && !b.pool_id.is_empty())
        .collect();
    if buckets.is_empty() {
        return Ok(());
    }
    let b = &buckets[rng.below(buckets.len() as u64) as usize];
    if !is_call_bucket(wrap, kind_cache, &b.bucket_id).await {
        return Ok(());
    }
    let pool_oid = ObjectID::from_hex_literal(&b.pool_id)?;
    let (_, lot, min) = sui_tx::tx::deepbook::derived_pool_params(
        b.asset_decimals.unwrap_or(9),
        b.settlement_decimals.unwrap_or(6),
    );
    let qty = (lot.saturating_mul(p.cfg.taker_size_lots.max(1))).max(min);

    // Sell when the taker holds base (from earlier buys); else buy.
    let base_held = bm_balance(
        &wrap.client,
        wrap.signer.address,
        &p.handles,
        bm_id,
        &b.call_coin_type,
    )
    .await
    .unwrap_or(0);
    let is_bid = base_held < qty || rng.below(10) < 7;

    if is_bid {
        // Budget ~generously: strike notional is the ceiling for a call
        // premium (qty × strike, u128 intermediate).
        let budget = settlement_ceiling(b, qty);
        let have = p
            .liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &b.settlement_coin_type, budget)
            .await;
        if have < budget {
            return Ok(());
        }
        // Deposit into the taker BM (owner-signed direct deposit).
        let mut pt = ProgrammableTransactionBuilder::new();
        let coin = gather_exact_coin(
            &wrap.client,
            &wrap.signer,
            &mut pt,
            &b.settlement_coin_type,
            budget,
        )
        .await?;
        let bm = pt.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
        pt.programmable_move_call(
            p.handles.package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![TypeTag::from_str(&b.settlement_coin_type)?],
            vec![bm, coin],
        );
        submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::taker_fund").await?;
        *spent = spent.saturating_add(budget);
    }

    // The market order + settled sweep, one PTB.
    let mut pt = ProgrammableTransactionBuilder::new();
    let pool = pt.obj(shared_object_arg(&wrap.client, pool_oid, true).await?)?;
    let bm = pt.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
    let proof = pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );
    let tags = vec![
        TypeTag::from_str(&b.call_coin_type)?,
        TypeTag::from_str(&b.settlement_coin_type)?,
    ];
    let clock = clock_arg(&mut pt)?;
    let a_client = pt.pure(now / 60_000)?; // client order id: unix minute
    let a_self = pt.pure(0u8)?;
    let a_qty = pt.pure(qty)?;
    let a_bid = pt.pure(is_bid)?;
    let a_deep = pt.pure(false)?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("place_market_order").unwrap(),
        tags.clone(),
        vec![pool, bm, proof, a_client, a_self, a_qty, a_bid, a_deep, clock],
    );
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("withdraw_settled_amounts_permissionless").unwrap(),
        tags,
        vec![pool, bm],
    );
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::taker_order").await?;
    info!(pool = %b.pool_id, qty, is_bid, "[sim] taker crossed");
    Ok(())
}

/// qty × strike / 10^strike_scale, saturating — the most settlement a
/// call purchase of `qty` could conceivably cost.
fn settlement_ceiling(b: &TradeableBucket, qty: u64) -> u64 {
    let scale = 10u128.saturating_pow(b.strike_scale as u32).max(1);
    let v = (qty as u128).saturating_mul(b.strike_raw) / scale;
    u64::try_from(v).unwrap_or(u64::MAX).max(1_000_000)
}

async fn wallet_balance(wrap: &SuiClientWrapper, coin_type: &str) -> u64 {
    wrap.client
        .coin_read_api()
        .get_balance(wrap.signer.address, Some(coin_type.to_string()))
        .await
        .map(|b| u64::try_from(b.total_balance).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
