//! Exchange listings engine (SO-416): the desk EXITS option inventory by
//! resting ASK orders on the in-house exchange (orderbook service + the
//! on-chain `exchange` package), replacing the retired DeepBook taker
//! resale. Once an ask rests, the exchange's own matching engine executes
//! the resale whenever a bid crosses — the desk needs no taker leg.
//!
//! Per desk-refresher-cadence tick, for every holding with vault-side
//! units, an unexpired bucket and a model mark:
//!
//!   1. **Market**: resolve the option's market from `/v1/markets` by
//!      canonical base type; when absent, permissionlessly list one via
//!      `exchange_listing::create_*_market` (the dedup `EAlreadyListed`
//!      abort is success-equivalent).
//!   2. **Provisioning** (once): the vault's DIRECT `ExchangeCustody` +
//!      identity BalanceManager (`exchange_adapter::init_direct_custody`
//!      + `vault::add_quote_adapter`), this wallet delegated as an
//!      approved order signer, and the option coin on the vault's
//!      deposit-asset allowlist. Coin-custody positions are moved into
//!      free balances (`vault_mm::release_coin_to_balances`) so fills can
//!      settle from them.
//!   3. **One resting ask per holding** (levels = 1): price = mark ×
//!      (1 + concession), snapped UP to the tick grid; size = vault units
//!      × max_fraction, snapped DOWN to lots. Cancel-replace on price
//!      drift, size change or approaching order expiry; orders expire at
//!      `min(now + ttl, bucket expiry)`.
//!
//! Listed units are recorded on the book's ledger
//! ([`Book::set_listed_units`]) so exits/quoting never double-commit the
//! same inventory. Soft-cancelled orders stay fillable on-chain until a
//! periodic salt-watermark sweep (`settlement::cancel_up_to_for_manager`,
//! batched) voids them — the short order TTL bounds the exposure, so the
//! watermark is a slow belt, not the primary cancel.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use parking_lot::RwLock;
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use exchange_types::{Market, Order, SuiAddress as ExAddress};
use indexer_graphql::IndexerClient;
use protocol_types::ids::ObjectId;
use sui_tx::chain::{created_objects, decode_return_value};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::exchange as exchange_tx;
use sui_tx::tx::{owned_object_arg, shared_object_arg, submit_ptb, submit_ptb_rebuilding};

use super::book::{Book, Holding};
use super::exchange_client::{IntakeReject, OrderbookClient, OrderSigner};
use super::{CuratorRefs, DeskShared};

const ALERT_ID: &str = "tx-failed-mm-bot-listings";

/// `exchange_listing` abort codes that are success-equivalent for a
/// permissionless lister (see `contracts/exchange-listing`).
const LISTING_EXPIRED_SERIES: u64 = 4;
const LISTING_ALREADY_LISTED: u64 = 5;

/// `[desk.listings]` — resting-ask exit engine knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ListingsConfig {
    pub enabled: bool,
    /// Base URL of the orderbook service (markets, order intake, cancels).
    pub orderbook_url: String,
    /// Asks list ABOVE fair: price = mark × (1 + concession_bps/10000).
    pub concession_bps: u64,
    /// Cancel-replace when |price drift| exceeds this many bps of the
    /// resting price.
    pub requote_drift_bps: u64,
    /// Order lifetime; also bounded by the bucket's own expiry.
    pub order_ttl_secs: u64,
    /// Fraction of a holding's vault units to list, bps (10000 = all).
    pub max_fraction_bps: u64,
    /// Skip listings below this many underlying raw units.
    pub min_list_units: u64,
    /// Per-order signed fee ceiling (chain caps at 50).
    pub max_fee_bps: u64,
    /// Cadence of the batched on-chain watermark sweep. Safe to keep
    /// slow: replaced orders expire on-chain after `order_ttl_secs`.
    pub watermark_interval_secs: u64,
    pub gas_budget: u64,
}

impl Default for ListingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            orderbook_url: "http://127.0.0.1:9014".into(),
            concession_bps: 200,
            requote_drift_bps: 50,
            order_ttl_secs: 900,
            max_fraction_bps: 10_000,
            min_list_units: 0,
            max_fee_bps: 50,
            watermark_interval_secs: 3_600,
            gas_budget: 200_000_000,
        }
    }
}

/// One holding's resting ask, published for `/desk/state`.
#[derive(Clone, Debug)]
pub struct ListingSnapshot {
    pub market_registry_id: String,
    pub market_symbol: String,
    pub digest: String,
    pub price_ticks: u64,
    /// Quote raw units per base raw unit (`ticks × tick_size / lot_size`).
    pub price_per_unit: f64,
    /// Base (underlying raw) units resting.
    pub size_units: u64,
    pub order_expiry_ms: u64,
    pub at_ms: u64,
}

// ── pure ask math ──────────────────────────────────────────────────────

/// Ask price in ticks: mark lifted by the concession, snapped UP to the
/// market's tick grid (an ask never rounds below its target price).
/// `mark_per_unit` is quote raw units per base raw unit — the same scale
/// as `MarkSnapshot::mark_per_unit`.
pub fn ask_price_ticks(mark_per_unit: f64, concession_bps: u64, market: &Market) -> Option<u64> {
    if !mark_per_unit.is_finite() || mark_per_unit <= 0.0 || market.tick_size == 0 {
        return None;
    }
    let px = mark_per_unit * (1.0 + concession_bps as f64 / 10_000.0);
    let ticks = px * market.lot_size as f64 / market.tick_size as f64;
    if !ticks.is_finite() || ticks <= 0.0 || ticks > u64::MAX as f64 / 2.0 {
        return None;
    }
    let t = ticks.ceil() as u64;
    (t > 0).then_some(t)
}

/// Ask size in lots: `vault_units × max_fraction`, snapped DOWN to whole
/// lots; `None` when below the configured floor or the market minimum.
pub fn ask_size_lots(
    vault_units: u64,
    max_fraction_bps: u64,
    min_list_units: u64,
    market: &Market,
) -> Option<u64> {
    if market.lot_size == 0 {
        return None;
    }
    let units =
        ((vault_units as u128) * (max_fraction_bps.min(10_000) as u128) / 10_000) as u64;
    if units == 0 || units < min_list_units {
        return None;
    }
    let lots = units / market.lot_size;
    let base = lots.checked_mul(market.lot_size)?;
    if lots == 0 || base < market.min_size {
        return None;
    }
    Some(lots)
}

/// Cancel-replace decision for a resting ask: size changed, order expiry
/// inside the last third of the TTL, or price drifted more than
/// `drift_bps` off the resting price.
#[allow(clippy::too_many_arguments)]
pub fn should_requote(
    prev_ticks: u64,
    prev_lots: u64,
    prev_expiry_ms: u64,
    new_ticks: u64,
    new_lots: u64,
    drift_bps: u64,
    ttl_secs: u64,
    now_ms: u64,
) -> bool {
    if new_lots != prev_lots || prev_ticks == 0 {
        return true;
    }
    if prev_expiry_ms.saturating_sub(now_ms) < ttl_secs * 1_000 / 3 {
        return true;
    }
    (new_ticks.abs_diff(prev_ticks) as u128) * 10_000
        >= (prev_ticks as u128) * drift_bps as u128
}

/// Assemble the wire ask. Amounts are exact on the `(tick, lot)` grid by
/// construction (`base = lots × lot`, `quote = lots × ticks × tick`), so
/// the book's divisibility check holds; `None` on overflow past the
/// intake AMOUNT_RANGE bound.
#[allow(clippy::too_many_arguments)]
pub fn make_ask_order(
    price_ticks: u64,
    lots: u64,
    market: &Market,
    maker: ExAddress,
    manager: ExAddress,
    max_fee_bps: u64,
    expiry_ms: u64,
    salt: u64,
) -> Option<Order> {
    let base = (lots as u128).checked_mul(market.lot_size as u128)?;
    let quote = (lots as u128)
        .checked_mul(price_ticks as u128)?
        .checked_mul(market.tick_size as u128)?;
    let cap = i64::MAX as u128;
    if base == 0 || quote == 0 || base > cap || quote > cap {
        return None;
    }
    Some(Order {
        maker_token: market.base.clone(),
        taker_token: market.quote.clone(),
        maker_amount: base as u64,
        taker_amount: quote as u64,
        max_fee_bps,
        maker,
        maker_manager_id: manager,
        taker: ExAddress::ZERO,
        sender: ExAddress::ZERO,
        expiry_ms,
        salt,
    })
}

// ── the listings task ──────────────────────────────────────────────────

pub struct ListingsParams {
    pub cfg: ListingsConfig,
    /// Tick cadence — the desk refresher's `refresh_secs`.
    pub refresh_secs: u64,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub book: Arc<RwLock<Book>>,
    pub shared: Arc<DeskShared>,
    /// Curator refs — required: provisioning the direct custody and
    /// releasing coin positions are curator-session PTBs.
    pub curator: Option<CuratorRefs>,
    pub vault_protocol_config: ObjectID,
    /// exchange_adapter package (direct custody + signer delegation).
    pub exchange_adapter_package: Option<ObjectID>,
    /// exchange_listing package + shared ListingAuthority (permissionless
    /// market listing). Absent ⇒ existing markets only.
    pub exchange_listing_package: Option<ObjectID>,
    pub exchange_listing_authority: Option<ObjectID>,
    pub indexer_url: String,
}

pub fn spawn_listings(p: ListingsParams) {
    if !p.cfg.enabled {
        return;
    }
    let Some(curator) = p.curator else {
        tracing::warn!(
            "[desk.listings] enabled but curator refs unresolved — cannot provision the \
             vault's exchange custody; listings off"
        );
        return;
    };
    let Some(adapter_pkg) = p.exchange_adapter_package else {
        tracing::warn!(
            "[desk.listings] enabled but token-info has no exchangeAdapter package; listings off"
        );
        return;
    };
    tokio::spawn(async move {
        let wrap = match SuiClientWrapper::connect(&p.secrets, p.network).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "listings: sui connect failed; task exiting");
                return;
            }
        };
        let signer = match p
            .secrets
            .sui_private_key(p.network.as_str())
            .and_then(OrderSigner::from_sui_bech32)
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "listings: order signer unavailable; task exiting");
                return;
            }
        };
        if signer.address().to_hex() != wrap.signer.address.to_string() {
            tracing::error!(
                order_signer = %signer.address().to_hex(),
                wallet = %wrap.signer.address,
                "listings: order-signer address != wallet address; task exiting"
            );
            return;
        }
        let mut engine = Engine {
            ob: OrderbookClient::new(&p.cfg.orderbook_url),
            indexer: IndexerClient::new(p.indexer_url.clone()),
            wrap,
            signer,
            curator,
            adapter_pkg,
            listing: p
                .exchange_listing_package
                .zip(p.exchange_listing_authority),
            vault_protocol_config: p.vault_protocol_config,
            cfg: p.cfg.clone(),
            book: Arc::clone(&p.book),
            shared: Arc::clone(&p.shared),
            direct: None,
            allowlisted: HashSet::new(),
            released_positions: HashSet::new(),
            resting: HashMap::new(),
            recovered: false,
            listing_attempted_at: HashMap::new(),
            salts: SaltSource::new(),
            watermarks: HashMap::new(),
            last_sweep: std::time::Instant::now(),
        };
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            p.refresh_secs.max(15),
        ));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) = engine.tick().await {
                tracing::warn!(error = %format!("{e:#}"), "listings tick errored");
            }
            engine.maybe_sweep_watermarks().await;
        }
    });
}

/// Strictly-increasing salt source: time-seeded so a restart resumes
/// above everything a previous run issued (salts are per-(maker,
/// registry) monotonic forever on the books).
struct SaltSource(AtomicU64);

impl SaltSource {
    fn new() -> Self {
        Self(AtomicU64::new(super::auctions::now_ms() * 1_000))
    }
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// Resolved vault-direct escrow wiring (once per boot).
struct DirectCtx {
    exchange_package: ObjectID,
    /// The vault's direct-custody identity BalanceManager — the manager
    /// every signed ask names.
    manager_oid: ObjectID,
    manager: ExAddress,
    /// The order's `maker`: the vault's id-as-address (the identity BM's
    /// order-attribution owner).
    maker: ExAddress,
}

struct RestingAsk {
    digest: exchange_types::Digest,
    salt: u64,
    price_ticks: u64,
    lots: u64,
    /// Base units resting (`lots × lot_size`).
    size_units: u64,
    order_expiry_ms: u64,
    registry: ObjectID,
    base_type: String,
    quote_type: String,
}

/// Per-market watermark bookkeeping for the batched sweep (mirrors
/// staging-mm-bot).
struct MarketWatermark {
    base: String,
    quote: String,
    pending: Option<u64>,
    raised: u64,
}

struct Engine {
    ob: OrderbookClient,
    indexer: IndexerClient,
    wrap: SuiClientWrapper,
    signer: OrderSigner,
    curator: CuratorRefs,
    adapter_pkg: ObjectID,
    listing: Option<(ObjectID, ObjectID)>,
    vault_protocol_config: ObjectID,
    cfg: ListingsConfig,
    book: Arc<RwLock<Book>>,
    shared: Arc<DeskShared>,
    direct: Option<DirectCtx>,
    /// Canonical coin types already on the vault's deposit allowlist.
    allowlisted: HashSet<String>,
    /// Coin-custody position ids already released to free balances —
    /// the book's custody view lags a few ticks, and re-releasing a gone
    /// position aborts.
    released_positions: HashSet<ObjectId>,
    /// bucket_id → resting ask.
    resting: HashMap<ObjectId, RestingAsk>,
    /// Whether open orders were re-adopted after a restart.
    recovered: bool,
    /// Canonical base type → last create_option_market attempt (ms), so a
    /// stuck listing doesn't spam a tx per tick.
    listing_attempted_at: HashMap<String, u64>,
    salts: SaltSource,
    watermarks: HashMap<ObjectID, MarketWatermark>,
    last_sweep: std::time::Instant,
}

impl Engine {
    async fn tick(&mut self) -> Result<()> {
        let now = super::auctions::now_ms();
        let markets = self.ob.markets().await.context("orderbook /v1/markets")?;
        let exchange_package = ObjectID::from_str(&markets.package_id)
            .context("parsing exchange packageId from /v1/markets")?;
        if self.direct.is_none() {
            let ctx = self.ensure_direct(exchange_package).await?;
            self.direct = Some(ctx);
        }
        // Markets keyed by canonical base type (the option coin).
        let by_base: HashMap<&str, &Market> =
            markets.markets.iter().map(|m| (m.base.as_str(), m)).collect();

        if !self.recovered {
            self.recover_open_orders(&by_base, now).await;
            self.recovered = true;
        }

        let holdings: Vec<Holding> = self.book.read().holdings.clone();
        let marks = self.shared.marks.read().clone();
        let mut live: HashSet<ObjectId> = HashSet::new();

        for h in &holdings {
            let vault_units = h.amount_vault.saturating_add(h.amount_coin_positions());
            if vault_units == 0 || h.expiry_ms <= now {
                continue;
            }
            let Ok(base) = exchange_types::canonicalize_move_type(&h.option_coin_type) else {
                tracing::warn!(coin = %h.option_coin_type, "listings: uncanonicalizable option coin");
                continue;
            };
            let Some(mark) = marks.get(&h.bucket_id).map(|m| m.mark_per_unit) else {
                continue; // no model mark this tick — leave any resting ask alone
            };
            let Some(market) = by_base.get(base.as_str()).copied() else {
                self.maybe_create_market(h, &base, now).await;
                continue; // market shows up in /v1/markets within seconds
            };
            live.insert(h.bucket_id);
            if let Err(e) = self.maintain_ask(h, market, mark, vault_units, now).await {
                tracing::warn!(
                    bucket = %h.bucket_id.to_hex(),
                    error = %format!("{e:#}"),
                    "listings: maintaining ask failed"
                );
            }
        }

        // Pull asks whose holding is gone/expired/unmarked-out (fills and
        // custody re-syncs shrink holdings out-of-band).
        let stale: Vec<ObjectId> = self
            .resting
            .keys()
            .filter(|b| !live.contains(*b))
            .copied()
            .collect();
        for bucket in stale {
            self.pull_ask(&bucket).await;
        }
        metrics::gauge!("mm_desk_listings_resting").set(self.resting.len() as f64);
        Ok(())
    }

    /// Maintain the single resting ask for one holding: place, hold, or
    /// cancel-replace.
    async fn maintain_ask(
        &mut self,
        h: &Holding,
        market: &Market,
        mark_per_unit: f64,
        vault_units: u64,
        now: u64,
    ) -> Result<()> {
        let desired = ask_price_ticks(mark_per_unit, self.cfg.concession_bps, market)
            .zip(ask_size_lots(
                vault_units,
                self.cfg.max_fraction_bps,
                self.cfg.min_list_units,
                market,
            ));
        let Some((ticks, lots)) = desired else {
            // Nothing listable (dust / off-grid) — pull any resting ask.
            if self.resting.contains_key(&h.bucket_id) {
                self.pull_ask(&h.bucket_id).await;
            }
            return Ok(());
        };
        if let Some(prev) = self.resting.get(&h.bucket_id) {
            if !should_requote(
                prev.price_ticks,
                prev.lots,
                prev.order_expiry_ms,
                ticks,
                lots,
                self.cfg.requote_drift_bps,
                self.cfg.order_ttl_secs,
                now,
            ) {
                return Ok(());
            }
            self.cancel_resting(&h.bucket_id).await;
        }

        // Fills settle from vault FREE balances: coin-custody positions
        // must be released first, and the option coin must be on the
        // vault's deposit-asset allowlist.
        self.ensure_deposit_asset(&h.option_coin_type).await?;
        if !h.coin_positions.is_empty() {
            self.release_coin_positions(h).await?;
        }

        let expiry_ms = (now + self.cfg.order_ttl_secs * 1_000).min(h.expiry_ms);
        if expiry_ms <= now {
            return Ok(());
        }
        let direct = self.direct.as_ref().expect("direct ctx resolved before listing");
        let salt = self.salts.next();
        let Some(order) = make_ask_order(
            ticks,
            lots,
            market,
            direct.maker,
            direct.manager,
            self.cfg.max_fee_bps,
            expiry_ms,
            salt,
        ) else {
            return Ok(());
        };
        let size_units = order.maker_amount;
        let (digest, signed) = self.signer.sign_order(order, market.registry_id);
        match self.ob.place_order(&signed).await? {
            Ok(resp) => {
                metrics::counter!("mm_desk_listings_orders_placed_total").increment(1);
                if resp.status == "SELF_TRADE_CANCELLED" {
                    return Ok(());
                }
                tracing::info!(
                    bucket = %h.bucket_id.to_hex(),
                    market = %market.symbol,
                    price_ticks = ticks,
                    size_units,
                    "listed resting ask on exchange"
                );
                self.record_resting(
                    h.bucket_id,
                    RestingAsk {
                        digest,
                        salt,
                        price_ticks: ticks,
                        lots,
                        size_units,
                        order_expiry_ms: expiry_ms,
                        registry: ObjectID::from_str(&market.registry_id.to_hex())
                            .unwrap_or(ObjectID::ZERO),
                        base_type: market.base.clone(),
                        quote_type: market.quote.clone(),
                    },
                    market,
                    now,
                );
            }
            Err(IntakeReject { code, detail }) => {
                metrics::counter!("mm_desk_listings_orders_rejected_total", "code" => code.clone())
                    .increment(1);
                if code == "INSUFFICIENT_ESCROW" {
                    // Mirror lag (a fresh release) or stale resting
                    // commitment from a previous run — retry next tick.
                    tracing::warn!(bucket = %h.bucket_id.to_hex(), detail, "listings: escrow busy");
                } else {
                    tracing::error!(bucket = %h.bucket_id.to_hex(), code, detail, "listings: order rejected");
                }
            }
        }
        Ok(())
    }

    /// Record a resting ask + mirror it to the book ledger and the
    /// `/desk/state` snapshot.
    fn record_resting(&mut self, bucket: ObjectId, ask: RestingAsk, market: &Market, now: u64) {
        self.book.write().set_listed_units(bucket, ask.size_units);
        self.shared.listings.write().insert(
            bucket,
            ListingSnapshot {
                market_registry_id: market.registry_id.to_hex(),
                market_symbol: market.symbol.clone(),
                digest: ask.digest.to_hex(),
                price_ticks: ask.price_ticks,
                price_per_unit: ask.price_ticks as f64 * market.tick_size as f64
                    / market.lot_size as f64,
                size_units: ask.size_units,
                order_expiry_ms: ask.order_expiry_ms,
                at_ms: now,
            },
        );
        self.resting.insert(bucket, ask);
    }

    /// Soft-cancel a resting ask, queue its salt for the watermark
    /// sweep, and clear the book/state mirrors (a following
    /// `record_resting` reinstates them on the replacement).
    async fn cancel_resting(&mut self, bucket: &ObjectId) {
        self.book.write().set_listed_units(*bucket, 0);
        self.shared.listings.write().remove(bucket);
        let Some(ask) = self.resting.remove(bucket) else { return };
        let (sig, pk) = self.signer.sign_cancel(&ask.digest);
        if let Err(e) = self.ob.cancel_order(&ask.digest, &sig, &pk).await {
            // Best effort: FILLED orders 4xx here; the watermark voids
            // anything still resting.
            tracing::debug!(error = %format!("{e:#}"), "listings: soft cancel failed");
        }
        let w = self
            .watermarks
            .entry(ask.registry)
            .or_insert_with(|| MarketWatermark {
                base: ask.base_type.clone(),
                quote: ask.quote_type.clone(),
                pending: None,
                raised: 0,
            });
        if ask.salt > w.raised {
            w.pending = Some(w.pending.map_or(ask.salt, |p| p.max(ask.salt)));
        }
    }

    /// Cancel + drop every trace of a holding's ask (holding gone or no
    /// longer listable).
    async fn pull_ask(&mut self, bucket: &ObjectId) {
        self.cancel_resting(bucket).await;
    }

    /// One batched `cancel_up_to_for_manager` per sweep interval — voids
    /// every replaced/pulled ask across all markets in a single PTB.
    async fn maybe_sweep_watermarks(&mut self) {
        if self.last_sweep.elapsed().as_secs() < self.cfg.watermark_interval_secs {
            return;
        }
        self.last_sweep = std::time::Instant::now();
        let Some(direct) = self.direct.as_ref() else { return };
        let targets: Vec<exchange_tx::CancelUpToTarget> = self
            .watermarks
            .iter()
            .filter_map(|(registry, w)| {
                let salt = w.pending.filter(|p| *p > w.raised)?;
                Some(exchange_tx::CancelUpToTarget {
                    registry_id: *registry,
                    base_type: w.base.clone(),
                    quote_type: w.quote.clone(),
                    min_valid_salt: salt,
                    // The maker (watermark key) is the vault's address,
                    // which can never be a tx sender — route through the
                    // manager-keyed variant.
                    manager_id: Some(direct.manager_oid),
                })
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        match exchange_tx::cancel_up_to_batch(
            &self.wrap.client,
            &self.wrap.signer,
            direct.exchange_package,
            &targets,
            self.cfg.gas_budget,
        )
        .await
        {
            Ok(_) => {
                for t in &targets {
                    if let Some(w) = self.watermarks.get_mut(&t.registry_id) {
                        w.raised = w.raised.max(t.min_valid_salt);
                        if w.pending.is_some_and(|p| p <= w.raised) {
                            w.pending = None;
                        }
                    }
                }
            }
            Err(e) => {
                // A lagging watermark leaves soft-cancelled orders
                // fillable on-chain — alert, keep pending for the retry.
                tracing::error!(
                    alert_id = ALERT_ID,
                    markets = targets.len(),
                    error = %format!("{e:#}"),
                    "listings: batched cancel_up_to failed"
                );
            }
        }
    }

    /// Re-adopt asks that still rest from a previous run so a restart
    /// never double-lists the same inventory. Unmappable open asks
    /// (market or holding gone) are soft-cancelled.
    async fn recover_open_orders(&mut self, by_base: &HashMap<&str, &Market>, now: u64) {
        let Some(direct) = self.direct.as_ref() else { return };
        let maker = direct.maker;
        let orders = match self.ob.orders_by_account(&maker).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "listings: open-order recovery failed");
                return;
            }
        };
        let holdings: Vec<Holding> = self.book.read().holdings.clone();
        for entry in orders {
            if entry.status != "OPEN" || entry.order.order.expiry_ms <= now {
                continue;
            }
            let o = &entry.order.order;
            let Ok(digest) = exchange_types::Digest::parse(&entry.digest) else { continue };
            let market = by_base.get(o.maker_token.as_str()).copied();
            let holding = holdings.iter().find(|h| {
                exchange_types::canonicalize_move_type(&h.option_coin_type).ok().as_deref()
                    == Some(o.maker_token.as_str())
            });
            match (market, holding) {
                (Some(market), Some(h)) if market.lot_size > 0 => {
                    let lots = o.maker_amount / market.lot_size;
                    let ticks = if lots > 0 && market.tick_size > 0 {
                        o.taker_amount / lots / market.tick_size
                    } else {
                        0
                    };
                    tracing::info!(
                        bucket = %h.bucket_id.to_hex(),
                        digest = %digest.to_hex(),
                        "listings: re-adopted resting ask from a previous run"
                    );
                    self.record_resting(
                        h.bucket_id,
                        RestingAsk {
                            digest,
                            salt: o.salt,
                            price_ticks: ticks,
                            lots,
                            size_units: o.maker_amount,
                            order_expiry_ms: o.expiry_ms,
                            registry: ObjectID::from_str(&market.registry_id.to_hex())
                                .unwrap_or(ObjectID::ZERO),
                            base_type: market.base.clone(),
                            quote_type: market.quote.clone(),
                        },
                        market,
                        now,
                    );
                }
                _ => {
                    let (sig, pk) = self.signer.sign_cancel(&digest);
                    if let Err(e) = self.ob.cancel_order(&digest, &sig, &pk).await {
                        tracing::debug!(error = %format!("{e:#}"), "listings: orphan cancel failed");
                    }
                }
            }
        }
    }

    /// Permissionlessly list an exchange market for the holding's bucket.
    /// Rate-limited per base type; the `EAlreadyListed` dedup abort is
    /// success-equivalent (someone raced us), `EExpiredSeries` is a
    /// benign skip.
    async fn maybe_create_market(&mut self, h: &Holding, base: &str, now: u64) {
        let Some((listing_pkg, authority)) = self.listing else {
            return; // no listing package — existing markets only
        };
        if h.bucket_id == ObjectId::ZERO {
            return; // catalog-omitted holding: no bucket object to list from
        }
        const RETRY_MS: u64 = 600_000;
        if self
            .listing_attempted_at
            .get(base)
            .is_some_and(|at| now.saturating_sub(*at) < RETRY_MS)
        {
            return;
        }
        self.listing_attempted_at.insert(base.to_string(), now);
        match exchange_tx::create_option_market(
            &self.wrap.client,
            &self.wrap.signer,
            listing_pkg,
            authority,
            ObjectID::new(*h.bucket_id.as_bytes()),
            &h.option_coin_type,
            self.cfg.gas_budget,
        )
        .await
        {
            Ok(_) => {
                tracing::info!(bucket = %h.bucket_id.to_hex(), base, "listed exchange market for bucket");
                metrics::counter!("mm_desk_listings_markets_created_total").increment(1);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let code = crate::extract_abort_code(&msg);
                let in_listing = msg.contains("Identifier(\"exchange_listing\")");
                match code {
                    Some(LISTING_ALREADY_LISTED) if in_listing => {
                        // Dedup race — the market exists; discovery picks
                        // it up next tick.
                        tracing::info!(bucket = %h.bucket_id.to_hex(), "market already listed (dedup)");
                    }
                    Some(LISTING_EXPIRED_SERIES) if in_listing => {
                        tracing::debug!(bucket = %h.bucket_id.to_hex(), "series expired; not listable");
                    }
                    _ => {
                        tracing::error!(
                            alert_id = ALERT_ID,
                            bucket = %h.bucket_id.to_hex(),
                            base,
                            error = %msg,
                            "create_option_market tx failed"
                        );
                    }
                }
            }
        }
    }

    // ── vault-direct provisioning (once) ───────────────────────────────

    /// Resolve (or create) the vault's DIRECT exchange custody + identity
    /// BM, ensure this wallet is an approved order signer, and seed the
    /// deposit-asset allowlist cache. Modeled on staging-mm-bot's `vault`
    /// module; the vault + CuratorCap are already proven by the desk's
    /// provision step.
    async fn ensure_direct(&mut self, exchange_package: ObjectID) -> Result<DirectCtx> {
        let refs = self.curator;
        let vault_id = refs.vault_id;
        let custody = match self.direct_custody(vault_id).await? {
            Some(c) => c,
            None => self.wire_direct_custody().await?,
        };
        let me = self.wrap.signer.address;
        if !self.is_approved_signer(exchange_package, custody.bm_id, me).await? {
            self.add_signer(&custody).await?;
        }
        for t in self.deposit_assets().await? {
            self.allowlisted.insert(t);
        }
        let manager = ExAddress::parse(&custody.bm_id.to_string())
            .map_err(|e| anyhow!("manager id hex: {e}"))?;
        let maker = ExAddress::parse(&vault_id.to_string())
            .map_err(|e| anyhow!("vault id hex: {e}"))?;
        tracing::info!(
            vault = %vault_id.to_hex_literal(),
            manager = %custody.bm_id,
            %exchange_package,
            "listings: vault direct escrow ready (fills settle from vault free balances)"
        );
        Ok(DirectCtx { exchange_package, manager_oid: custody.bm_id, manager, maker })
    }

    /// The vault's direct custody, from the indexer's durable event store
    /// (RPC event history prunes — SO-369).
    async fn direct_custody(&self, vault_id: ObjectID) -> Result<Option<Custody>> {
        let hex = ObjectId::new(vault_id.into_bytes()).to_hex();
        let events = self
            .indexer
            .recent_events_with_payload(
                &["TvExchangeCustodyCreated"],
                serde_json::json!({ "vault_id": hex, "direct": true }),
                16,
            )
            .await
            .context("querying ExchangeCustodyCreated events")?;
        for ev in events.iter().rev() {
            if let protocol_types::events::ChainEvent::TvExchangeCustodyCreated(c) = &ev.event {
                if c.direct {
                    return Ok(Some(Custody {
                        custody_id: ObjectID::new(*c.custody_id.as_bytes()),
                        bm_id: ObjectID::new(*c.balance_manager_id.as_bytes()),
                    }));
                }
            }
        }
        Ok(None)
    }

    /// `init_direct_custody` + `add_quote_adapter`, one PTB — atomic, so
    /// "custody exists" implies "adapter enabled".
    async fn wire_direct_custody(&self) -> Result<Custody> {
        let refs = &self.curator;
        let client = &self.wrap.client;
        let witness = TypeTag::from_str(&format!(
            "{}::exchange_adapter::ExchangeAdapter",
            self.adapter_pkg
        ))?;
        let resp = submit_ptb_rebuilding(
            client,
            &self.wrap.signer,
            self.cfg.gas_budget,
            "exchange_adapter::init_direct_custody",
            || async {
                let mut pt = ProgrammableTransactionBuilder::new();
                let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
                let cap = pt.obj(owned_object_arg(client, refs.curator_cap).await?)?;
                let reg =
                    pt.obj(shared_object_arg(client, refs.integration_registry, false).await?)?;
                pt.programmable_move_call(
                    self.adapter_pkg,
                    Identifier::new("exchange_adapter").unwrap(),
                    Identifier::new("init_direct_custody").unwrap(),
                    vec![],
                    vec![vault, cap, reg],
                );
                pt.programmable_move_call(
                    refs.trading_vault_package,
                    Identifier::new("vault").unwrap(),
                    Identifier::new("add_quote_adapter").unwrap(),
                    vec![witness.clone()],
                    vec![vault, cap],
                );
                Ok(pt.finish())
            },
        )
        .await
        .context("initializing the vault's direct exchange custody")?;
        let (mut custody_id, mut bm_id) = (None, None);
        for c in created_objects(&resp) {
            let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
            match tag.name.as_str() {
                "ExchangeCustody" => custody_id = Some(c.object_id),
                "BalanceManager" => bm_id = Some(c.object_id),
                _ => {}
            }
        }
        let custody = Custody {
            custody_id: custody_id.ok_or_else(|| anyhow!("no ExchangeCustody created"))?,
            bm_id: bm_id.ok_or_else(|| anyhow!("no identity BalanceManager created"))?,
        };
        client
            .await_object(custody.bm_id, 6)
            .await
            .context("waiting for the identity BM to be readable")?;
        tracing::info!(
            vault = %refs.vault_id.to_hex_literal(),
            custody = %custody.custody_id,
            manager = %custody.bm_id,
            "listings: direct custody initialized, quote adapter enabled"
        );
        Ok(custody)
    }

    /// Dev-inspect `balance_manager::is_approved_signer` on the live
    /// exchange package.
    async fn is_approved_signer(
        &self,
        exchange_package: ObjectID,
        bm_id: ObjectID,
        addr: sui_types::base_types::SuiAddress,
    ) -> Result<bool> {
        let client = &self.wrap.client;
        let mut pt = ProgrammableTransactionBuilder::new();
        let bm = pt.obj(shared_object_arg(client, bm_id, false).await?)?;
        let a = pt.pure(addr)?;
        pt.programmable_move_call(
            exchange_package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("is_approved_signer").unwrap(),
            vec![],
            vec![bm, a],
        );
        let res = client
            .dev_inspect_ptb(self.wrap.signer.address, pt)
            .await
            .context("dev-inspecting is_approved_signer")?;
        decode_return_value::<bool>(&res, 0).context("decoding is_approved_signer")
    }

    /// Delegate this wallet as an order-signing hot key on the identity BM.
    async fn add_signer(&self, custody: &Custody) -> Result<()> {
        let refs = &self.curator;
        let client = &self.wrap.client;
        let delegate = self.wrap.signer.address;
        submit_ptb_rebuilding(
            client,
            &self.wrap.signer,
            self.cfg.gas_budget,
            "exchange_adapter::add_signer",
            || async {
                let mut pt = ProgrammableTransactionBuilder::new();
                let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
                let cap = pt.obj(owned_object_arg(client, refs.curator_cap).await?)?;
                let reg =
                    pt.obj(shared_object_arg(client, refs.integration_registry, false).await?)?;
                let bm = pt.obj(shared_object_arg(client, custody.bm_id, true).await?)?;
                let custody_arg = pt.pure(custody.custody_id)?;
                let signer = pt.pure(delegate)?;
                pt.programmable_move_call(
                    self.adapter_pkg,
                    Identifier::new("exchange_adapter").unwrap(),
                    Identifier::new("add_signer").unwrap(),
                    vec![],
                    vec![vault, cap, reg, bm, custody_arg, signer],
                );
                Ok(pt.finish())
            },
        )
        .await
        .context("delegating this wallet on the identity BM")?;
        tracing::info!(vault = %refs.vault_id.to_hex_literal(), %delegate, "listings: delegated as approved order signer");
        Ok(())
    }

    /// Dev-inspect `vault::deposit_assets` — the current allowlist as
    /// canonical coin-type strings.
    async fn deposit_assets(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct TypeName {
            name: String,
        }
        #[derive(Deserialize)]
        struct VecSet {
            contents: Vec<TypeName>,
        }
        let refs = &self.curator;
        let client = &self.wrap.client;
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
        pt.programmable_move_call(
            refs.trading_vault_package,
            Identifier::new("vault").unwrap(),
            Identifier::new("deposit_assets").unwrap(),
            vec![],
            vec![vault],
        );
        let res = client
            .dev_inspect_ptb(self.wrap.signer.address, pt)
            .await
            .context("dev-inspecting deposit_assets")?;
        let set = decode_return_value::<VecSet>(&res, 0).context("decoding deposit_assets")?;
        // TypeName strings carry no 0x prefix — canonicalize (project rule).
        Ok(set
            .contents
            .iter()
            .map(|t| protocol_types::asset::canonicalize_move_type(&t.name))
            .collect())
    }

    /// Allowlist the option coin for vault deposits (fill settlement),
    /// once per coin type.
    async fn ensure_deposit_asset(&mut self, coin_type: &str) -> Result<()> {
        let canonical = protocol_types::asset::canonicalize_move_type(coin_type);
        if self.allowlisted.contains(&canonical) {
            return Ok(());
        }
        let refs = &self.curator;
        let client = &self.wrap.client;
        let tag = TypeTag::from_str(&canonical)
            .with_context(|| format!("parsing coin type {canonical}"))?;
        submit_ptb_rebuilding(
            client,
            &self.wrap.signer,
            self.cfg.gas_budget,
            "vault::add_deposit_asset",
            || async {
                let mut pt = ProgrammableTransactionBuilder::new();
                let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
                let cap = pt.obj(owned_object_arg(client, refs.curator_cap).await?)?;
                let cfg =
                    pt.obj(shared_object_arg(client, self.vault_protocol_config, false).await?)?;
                pt.programmable_move_call(
                    refs.trading_vault_package,
                    Identifier::new("vault").unwrap(),
                    Identifier::new("add_deposit_asset").unwrap(),
                    vec![tag.clone()],
                    vec![vault, cap, cfg],
                );
                Ok(pt.finish())
            },
        )
        .await
        .context("allowlisting the option coin as a vault deposit asset")?;
        tracing::info!(coin = %canonical, "listings: option coin allowlisted for vault deposits");
        self.allowlisted.insert(canonical);
        Ok(())
    }

    /// Move every not-yet-released coin-custody position of this holding
    /// into vault free balances (`vault_mm::release_coin_to_balances`,
    /// one PTB) so fills can settle from them. The book's custody view
    /// lags a few refresher ticks, so already-released ids are tracked
    /// and skipped.
    async fn release_coin_positions(&mut self, h: &Holding) -> Result<()> {
        let pending: Vec<ObjectId> = h
            .coin_positions
            .iter()
            .map(|cp| cp.position_id)
            .filter(|id| !self.released_positions.contains(id))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let refs = &self.curator;
        let mut pt = ProgrammableTransactionBuilder::new();
        let (vault, cap, reg) = self.curator_args(&mut pt).await?;
        let option_tag = TypeTag::from_str(&h.option_coin_type)?;
        for position_id in &pending {
            let coin_position_id = pt.pure(&ObjectID::new(*position_id.as_bytes()))?;
            pt.programmable_move_call(
                refs.trading_vault_package,
                Identifier::new("vault_mm").unwrap(),
                Identifier::new("release_coin_to_balances").unwrap(),
                vec![option_tag.clone()],
                vec![vault, cap, reg, coin_position_id],
            );
        }
        let resp = submit_ptb(
            &self.wrap.client,
            &self.wrap.signer,
            pt,
            self.cfg.gas_budget,
            "listings release coin positions",
        )
        .await
        .inspect_err(|e| {
            tracing::error!(
                alert_id = ALERT_ID,
                bucket = %h.bucket_id.to_hex(),
                positions = pending.len(),
                error = %format!("{e:#}"),
                "release_coin_to_balances tx failed"
            );
        })?;
        self.released_positions.extend(pending.iter().copied());
        tracing::info!(
            bucket = %h.bucket_id.to_hex(),
            positions = pending.len(),
            digest = %sui_tx::tx::tx_digest(&resp),
            "released coin-custody positions to vault free balances (fill settlement)"
        );
        Ok(())
    }

    /// The `(vault, cap, reg)` prefix every curator-session call takes —
    /// fetched fresh per PTB (each submit bumps the owned CuratorCap's
    /// version).
    async fn curator_args(
        &self,
        pt: &mut ProgrammableTransactionBuilder,
    ) -> Result<(Argument, Argument, Argument)> {
        let refs = &self.curator;
        let client = &self.wrap.client;
        let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
        let cap = pt.obj(owned_object_arg(client, refs.curator_cap).await?)?;
        let reg = pt.obj(shared_object_arg(client, refs.integration_registry, false).await?)?;
        Ok((vault, cap, reg))
    }
}

#[derive(Clone, Copy, Debug)]
struct Custody {
    custody_id: ObjectID,
    bm_id: ObjectID,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> Market {
        Market {
            symbol: "oTBTC/TUSDC".into(),
            registry_id: ExAddress::parse("0x5c").unwrap(),
            base: "0x00000000000000000000000000000000000000000000000000000000000000f8::option_coin::OptionCall"
                .into(),
            quote:
                "0x00000000000000000000000000000000000000000000000000000000000000f8::tusdc::TUSDC"
                    .into(),
            // tick 0.001 TUSDC per lot, lot = 1 whole base token (1e8),
            // min = lot/1000.
            tick_size: 1_000,
            min_size: 100_000,
            lot_size: 100_000_000,
            current_fee_bps: 10,
        }
    }

    #[test]
    fn ask_price_lists_above_fair_and_snaps_up() {
        let m = market();
        // mark 600 quote-raw/base-raw, +200 bps → 612 → ticks = 612 × 1e8
        // / 1e3 = 61_200_000 exactly.
        assert_eq!(ask_price_ticks(600.0, 200, &m), Some(61_200_000));
        // Zero concession keeps the mark on-grid.
        assert_eq!(ask_price_ticks(600.0, 0, &m), Some(60_000_000));
        // Off-grid marks snap UP, never below the target price.
        let ticks = ask_price_ticks(600.000004, 0, &m).unwrap();
        assert_eq!(ticks, 60_000_001);
        // Degenerate marks refuse.
        assert_eq!(ask_price_ticks(0.0, 200, &m), None);
        assert_eq!(ask_price_ticks(-1.0, 200, &m), None);
        assert_eq!(ask_price_ticks(f64::NAN, 200, &m), None);
    }

    #[test]
    fn ask_size_snaps_down_and_respects_floors() {
        let m = market();
        // 2.5 lots of units, full fraction → 2 whole lots.
        assert_eq!(ask_size_lots(250_000_000, 10_000, 0, &m), Some(2));
        // Half fraction of 4 lots → 2 lots.
        assert_eq!(ask_size_lots(400_000_000, 5_000, 0, &m), Some(2));
        // Below one lot but above min_size still refuses (lot floor).
        assert_eq!(ask_size_lots(50_000_000, 10_000, 0, &m), None);
        // min_list_units gate.
        assert_eq!(ask_size_lots(250_000_000, 10_000, 300_000_000, &m), None);
        // Zero units.
        assert_eq!(ask_size_lots(0, 10_000, 0, &m), None);
    }

    #[test]
    fn requote_triggers_on_drift_size_and_expiry() {
        let now = 1_000_000;
        let ttl = 900; // 900s → refresh inside the last 300s
        let expiry = now + 800_000;
        // Steady: same lots, tiny drift (< 50 bps), fresh order → hold.
        assert!(!should_requote(10_000, 3, expiry, 10_040, 3, 50, ttl, now));
        // Drift at/over 50 bps requotes.
        assert!(should_requote(10_000, 3, expiry, 10_050, 3, 50, ttl, now));
        assert!(should_requote(10_000, 3, expiry, 9_950, 3, 50, ttl, now));
        // Size change requotes regardless of drift.
        assert!(should_requote(10_000, 3, expiry, 10_000, 2, 50, ttl, now));
        // Order expiry inside the last third of the TTL requotes.
        assert!(should_requote(10_000, 3, now + 200_000, 10_000, 3, 50, ttl, now));
        assert!(!should_requote(10_000, 3, now + 400_000, 10_000, 3, 50, ttl, now));
    }

    #[test]
    fn ask_order_amounts_are_exact_on_the_grid() {
        let m = market();
        let maker = ExAddress::parse("0x9f").unwrap();
        let manager = ExAddress::parse("0x71").unwrap();
        let o = make_ask_order(61_200_000, 2, &m, maker, manager, 50, 1_754_330_000_000, 7)
            .unwrap();
        assert_eq!(o.maker_token, m.base);
        assert_eq!(o.taker_token, m.quote);
        assert_eq!(o.maker_amount, 200_000_000); // 2 lots
        assert_eq!(o.taker_amount, 2 * 61_200_000 * 1_000); // lots × ticks × tick
        // Divisibility the book checks: quote × lot / (base × tick) exact.
        assert_eq!(
            (o.taker_amount as u128 * m.lot_size as u128)
                % (o.maker_amount as u128 * m.tick_size as u128),
            0
        );
        // Overflow refuses.
        assert!(make_ask_order(u64::MAX / 2, u64::MAX / 2, &m, maker, manager, 50, 1, 1)
            .is_none());
    }
}
