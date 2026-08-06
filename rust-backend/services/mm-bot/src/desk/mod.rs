//! The vol desk (SO-299): V1 delta-hedged long-vol fund, V2 two-sided
//! maker behind a config gate. Replaces every legacy strategy module.
//!
//! Standing product decision (doc 05): the bot trades ONLY as the trading
//! vault's curator — quotes route collateral from the vault
//! (`release_module = "vault_mm"`, outputs to the vault address), auction
//! winnings land in the vault, exits pay the vault. `spawn_desk` resolves
//! that vault through [`provision`] — adopting a pinned or self-created
//! one, or creating it — and refuses to start without a usable vault.

pub mod auctions;
pub mod book;
pub mod exits;
pub mod hedge;
pub mod limits;
pub mod model;
pub mod monitors;
pub mod provision;
pub mod quote;
pub mod state;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sui_types::base_types::{ObjectID, SuiAddress};

use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::sui_client::Network;
use sui_tx::tx::deepbook::DeepBookHandles;

use crate::pricing::{compute_spot_from_cache, Staleness};

use book::{Book, PnlLine};
use limits::{BookExposure, LimitsConfig};
use model::{MarketModel, SurfaceConfig, V1BidParams, V2Params};
use quote::{Decision, FlowContext, RfqInputs};

// ── config ─────────────────────────────────────────────────────────────

/// `[desk]` — the desk master config. Serde defaults are the 00-plan
/// starting parameters.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeskConfig {
    pub enabled: bool,
    /// The trading vault the desk curates. REQUIRED when enabled.
    pub vault_id: String,
    /// Operator attestation that the vault's curator has toggled the
    /// `vault_mm` release path on. REQUIRED true when enabled — the desk
    /// refuses to boot otherwise (quotes would revert on release).
    pub mm_release_enabled: bool,
    /// Expected holding period for a bought option (hedge-cost estimate
    /// in the bid), years. Default 3 weeks.
    pub expected_holding_years: f64,
    /// Per-underlying annualized staking yield (`carry_yield` — the BAW
    /// dividend rate). Symbol-keyed; unlisted symbols use 0.
    pub carry_yields: HashMap<String, f64>,
    /// P&L attribution JSONL sink.
    pub pnl_jsonl_path: String,
    /// State dir: kill-switch NAV history + paper-hedge state files.
    pub state_dir: String,
    /// Book refresh cadence (marks, greeks, NAV, kill switch).
    pub refresh_secs: u64,
    pub surface: SurfaceTomlConfig,
    pub limits: LimitsConfig,
    pub v1: V1Config,
    pub v2: V2Config,
    pub hedge: hedge::HedgeConfig,
    pub auctions: auctions::AuctionsConfig,
    pub exits: exits::ExitsConfig,
    pub monitors: monitors::MonitorsConfig,
    /// `[desk.provision]` — create a vault when there is none to adopt.
    pub provision: provision::ProvisionConfig,
}

impl Default for DeskConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vault_id: String::new(),
            mm_release_enabled: false,
            expected_holding_years: 21.0 / 365.0,
            carry_yields: HashMap::new(),
            pnl_jsonl_path: "services/mm-bot/state/desk-pnl.jsonl".into(),
            state_dir: "services/mm-bot/state".into(),
            refresh_secs: 60,
            surface: SurfaceTomlConfig::default(),
            limits: LimitsConfig::default(),
            v1: V1Config::default(),
            v2: V2Config::default(),
            hedge: hedge::HedgeConfig::default(),
            auctions: auctions::AuctionsConfig::default(),
            exits: exits::ExitsConfig::default(),
            monitors: monitors::MonitorsConfig::default(),
            provision: provision::ProvisionConfig::default(),
        }
    }
}

/// `[desk.surface]` — vol-surface shaping (00-plan Phase 1). `Serialize`
/// so `/desk/state` can echo the effective config (SO-348).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct SurfaceTomlConfig {
    /// Risk premium over realized vol, absolute vol.
    pub risk_premium: f64,
    pub skew: f64,
    pub convexity: f64,
    pub term_short_boost: f64,
    pub term_decay_years: f64,
    /// External BTC/ETH anchor ratio (fast-follow; None = off).
    pub anchor_ratio: Option<f64>,
    pub floor_vol: f64,
    pub cap_vol: f64,
    pub short_window_weight: f64,
    pub long_window_weight: f64,
}

impl Default for SurfaceTomlConfig {
    fn default() -> Self {
        Self {
            risk_premium: 0.05,
            skew: 0.0,
            convexity: 0.0,
            term_short_boost: 0.0,
            term_decay_years: 0.25,
            anchor_ratio: None,
            floor_vol: 0.10,
            cap_vol: 5.0,
            short_window_weight: 1.0,
            long_window_weight: 1.0,
        }
    }
}

impl From<SurfaceTomlConfig> for SurfaceConfig {
    fn from(c: SurfaceTomlConfig) -> Self {
        SurfaceConfig {
            risk_premium: c.risk_premium,
            skew: c.skew,
            convexity: c.convexity,
            term_short_boost: c.term_short_boost,
            term_decay_years: c.term_decay_years,
            anchor_ratio: c.anchor_ratio,
            floor_vol: c.floor_vol,
            cap_vol: c.cap_vol,
            short_window_weight: c.short_window_weight,
            long_window_weight: c.long_window_weight,
        }
    }
}

/// `[desk.v1]` — the V1 bid discipline (00-plan V1 starting parameters).
/// Vol points are annualized decimals (0.05 = 5 vol pts), matching
/// `pricing::desk`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct V1Config {
    /// Bid at fair − this much vol. 00-plan: 4–6 vol pts → 0.05.
    pub base_spread_volpts: f64,
    /// +1 vol pt (0.01) per (notional / 1% NAV).
    pub size_penalty_volpts_per_pct_nav: f64,
    /// ~quadratic beyond this %NAV of notional. 00-plan: 3.
    pub size_penalty_quadratic_from_pct: f64,
    /// Inventory penalty → this much vol as vega utilization reaches
    /// 100%. 00-plan: 10 vol pts → 0.10.
    pub inventory_penalty_max_volpts: f64,
    /// 00-plan: 60%.
    pub inventory_penalty_start_util: f64,
    /// Max single fill, % of NAV in premium. 00-plan: 5.
    pub max_single_fill_pct_nav: f64,
}

impl Default for V1Config {
    fn default() -> Self {
        Self {
            base_spread_volpts: 0.05,
            size_penalty_volpts_per_pct_nav: 0.01,
            size_penalty_quadratic_from_pct: 3.0,
            inventory_penalty_max_volpts: 0.10,
            inventory_penalty_start_util: 0.6,
            max_single_fill_pct_nav: 5.0,
        }
    }
}

impl From<V1Config> for V1BidParams {
    fn from(c: V1Config) -> Self {
        V1BidParams {
            base_spread_volpts: c.base_spread_volpts,
            size_penalty_volpts_per_pct_nav: c.size_penalty_volpts_per_pct_nav,
            size_penalty_quadratic_from_pct: c.size_penalty_quadratic_from_pct,
            inventory_penalty_max_volpts: c.inventory_penalty_max_volpts,
            inventory_penalty_start_util: c.inventory_penalty_start_util,
            max_single_fill_pct_nav: c.max_single_fill_pct_nav,
        }
    }
}

/// `[desk.v2]` — the two-sided maker (00-plan V2 starting parameters).
/// Disabled by default; trader-flow RFQs decline while off.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct V2Config {
    pub enabled: bool,
    /// ±3 vol pt base spread (0.03 — netting allows tighter than V1).
    pub base_spread_volpts: f64,
    /// Signed vega band: +0.5% NAV/vol pt long…
    pub vega_band_long: f64,
    /// …to −0.15% NAV/vol pt short.
    pub vega_band_short: f64,
    /// Inventory-skew strength: mid shift = k × (net vega / band width),
    /// in vol (decimal). NEGATIVE k shifts the mid down when long (sell
    /// inventory) and up when short (buy it back) — the useful direction.
    pub skew_k: f64,
    /// Asymmetric size caps, % NAV: write 3 / buy 5.
    pub write_cap_pct_nav: f64,
    pub buy_cap_pct_nav: f64,
    /// Near-expiry ATM throttle: inside 48h, 2× spread and ½ size.
    pub near_expiry_hours: f64,
    pub near_expiry_spread_mult: f64,
    pub near_expiry_size_mult: f64,
    /// Naked short vega hard cap, NAV fraction per vol pt. 00-plan: 0.1%.
    pub naked_vega_cap_nav_per_volpt: f64,
}

impl Default for V2Config {
    fn default() -> Self {
        Self {
            enabled: false,
            base_spread_volpts: 0.03,
            vega_band_long: 0.005,
            vega_band_short: 0.0015,
            skew_k: -0.01,
            write_cap_pct_nav: 3.0,
            buy_cap_pct_nav: 5.0,
            near_expiry_hours: 48.0,
            near_expiry_spread_mult: 2.0,
            near_expiry_size_mult: 0.5,
            naked_vega_cap_nav_per_volpt: 0.001,
        }
    }
}

impl From<V2Config> for V2Params {
    fn from(c: V2Config) -> Self {
        V2Params {
            base_spread_volpts: c.base_spread_volpts,
            vega_band_long: c.vega_band_long,
            vega_band_short: c.vega_band_short,
            skew_k: c.skew_k,
            write_cap_pct_nav: c.write_cap_pct_nav,
            buy_cap_pct_nav: c.buy_cap_pct_nav,
            near_expiry_hours: c.near_expiry_hours,
            near_expiry_spread_mult: c.near_expiry_spread_mult,
            near_expiry_size_mult: c.near_expiry_size_mult,
        }
    }
}

// ── shared runtime state ───────────────────────────────────────────────

/// Per-bucket mark snapshot written by the book refresher: model fair,
/// the sigma/spot it was computed at, and per-unit greeks. Read by the
/// `/desk/state` endpoint (SO-348) so serving a snapshot never re-prices.
#[derive(Clone, Copy, Debug)]
pub struct MarkSnapshot {
    pub mark_per_unit: f64,
    pub sigma: f64,
    pub spot: f64,
    pub greeks: model::Greeks,
    pub at_ms: u64,
}

/// Per-symbol spot written each refresher tick.
#[derive(Clone, Copy, Debug)]
pub struct SpotSnapshot {
    pub spot: f64,
    pub at_ms: u64,
}

/// Last nightly-stress result (written by `monitors::stress_tick`).
#[derive(Clone, Copy, Debug, Default)]
pub struct StressSnapshot {
    pub at_ms: u64,
    pub gap_down_60: f64,
    pub gap_up_80: f64,
    pub flat_6mo: f64,
    pub funding_minus_50: f64,
    pub worst_drawdown: f64,
    pub blocked: bool,
}

/// State the desk's tasks share (the book refresher writes; quoting,
/// auctions and monitors read).
pub struct DeskShared {
    pub exposure: RwLock<BookExposure>,
    /// Net book delta per underlying coin type, underlying raw units.
    pub book_delta_units: RwLock<HashMap<String, f64>>,
    /// Naked written units across the book (V2 budget usage).
    pub naked_written_units: RwLock<u64>,
    /// Last observed hedge funding rate (annualized).
    pub funding_rate_annual: RwLock<f64>,
    /// Nightly stress gate: block new short risk (V2 §7).
    pub stress_blocked: AtomicBool,
    pub expected_holding_years: f64,
    pub slippage_bps: f64,
    /// Per-bucket marks + per-unit greeks from the last refresher tick
    /// (`/desk/state` reads; the refresher writes).
    pub marks: RwLock<HashMap<protocol_types::ids::ObjectId, MarkSnapshot>>,
    /// Per-symbol spot from the last refresher tick.
    pub spots: RwLock<HashMap<String, SpotSnapshot>>,
    /// Last stress-suite result (per-scenario drawdowns + the gate).
    pub stress: RwLock<Option<StressSnapshot>>,
}

impl DeskShared {
    pub async fn flow_context(&self, spot: f64) -> FlowContext {
        FlowContext {
            spot,
            exposure: self.exposure.read().clone(),
            funding_rate_annual: *self.funding_rate_annual.read(),
            expected_holding_years: self.expected_holding_years,
            slippage_bps: self.slippage_bps,
            naked_written_units: *self.naked_written_units.read(),
            stress_blocked: self.stress_blocked.load(Ordering::Relaxed),
        }
    }
}

// ── the desk handle (used by the WS serve loop) ────────────────────────

pub struct Desk {
    pub cfg: DeskConfig,
    /// The vault actually resolved at boot — the pinned one, a
    /// self-created one, or one this boot provisioned. Callers route
    /// collateral through this, NOT through `cfg.vault_id`, which is
    /// empty whenever the desk provisioned its own.
    pub vault_id: ObjectID,
    pub shared: Arc<DeskShared>,
    pub book: Arc<RwLock<Book>>,
    pub models: Arc<Vec<MarketModel>>,
    /// Every hedge-venue instance (each `[[desk.hedge.venues]]` spec ×
    /// underlying). `Arc` rather than `Box` because rebalancers and
    /// monitors share the instances.
    pub hedge_venues: Vec<Arc<dyn hedge::HedgeVenue>>,
    /// The same instances with their underlying symbol (the monitors'
    /// roster shape) — `/desk/state` reads positions/funding/margin
    /// through this (SO-348).
    pub venue_roster: Vec<monitors::MonitorVenue>,
    /// Whether this boot created the vault (vs adopting one).
    pub provisioned: bool,
    /// Curator refs when resolved; `None` = vault-funded bids and
    /// vault-custody exits are disabled (the silent-degradation signal
    /// `/desk/state` surfaces).
    pub curator_refs: Option<CuratorRefs>,
    pub booted_at_ms: u64,
    /// Static per-market metadata for `/desk/state` (aligned with
    /// `models`).
    pub market_meta: Vec<state::MarketMeta>,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    v1: V1BidParams,
    v2: Option<V2Params>,
    limits: LimitsConfig,
    quote_ttl_ms: u64,
}

impl Desk {
    /// Price one WS RFQ. `Side::Writer` = retail writes (the desk buys —
    /// V1 writer flow); `Side::Trader` = retail buys (the desk writes —
    /// V2 trader flow). With `reserve`, a writer-flow quote reserves its
    /// premium for the quote TTL (pass false for indicative bulk views —
    /// nothing is signed there).
    pub async fn price_ws_rfq(
        &self,
        side: Side,
        model_index: usize,
        inputs: RfqInputs,
        spot: f64,
        reserve: bool,
        now_ms: u64,
    ) -> Decision {
        let model = &self.models[model_index];
        let ctx = self.shared.flow_context(spot).await;
        match side {
            Side::Writer => {
                let d = quote::price_writer_flow(model, &self.v1, &self.limits, &ctx, &inputs, now_ms);
                if let (true, Decision::Quote { premium }) = (reserve, &d) {
                    // Reserve the premium while the quote is live; TTL
                    // expiry frees it if the quote is never executed.
                    if self
                        .book
                        .write()
                        .reserve(*premium, self.quote_ttl_ms, now_ms)
                        .is_err()
                    {
                        return Decision::Decline {
                            reason: "reservation ledger full (reservations + deployed ≥ NAV)".into(),
                        };
                    }
                }
                d
            }
            Side::Trader => {
                let cover = self.cover_available(&inputs);
                quote::price_trader_flow(
                    model,
                    self.v2.as_ref(),
                    self.cfg.v2.naked_vega_cap_nav_per_volpt,
                    &ctx,
                    &inputs,
                    cover,
                    now_ms,
                )
            }
        }
    }

    /// Held long units in the same series (strike/expiry/kind) available
    /// to cover a proposed write.
    fn cover_available(&self, inputs: &RfqInputs) -> u64 {
        let book = self.book.read();
        book.holdings
            .iter()
            .filter(|h| {
                h.is_put == inputs.is_put
                    && h.strike == inputs.strike
                    && h.strike_scale == inputs.strike_scale
                    && h.expiry_ms == inputs.expiry_ms
            })
            .map(|h| h.amount())
            .sum()
    }
}

// ── boot wiring ────────────────────────────────────────────────────────

/// One market's chassis feed, handed over from `main`.
pub struct DeskMarket {
    pub symbol: String,
    pub coin_type: String,
    pub feed: PriceFeedId,
    pub decimals: u8,
    pub vol_buf: Arc<RwLock<RollingVolBuffer>>,
    pub vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    pub fallback_vol: f64,
}

pub struct DeskParams {
    pub cfg: DeskConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub markets: Vec<DeskMarket>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub staleness: Staleness,
    pub price_cache: PriceCache,
    pub api_url: String,
    pub indexer_url: String,
    pub rate: f64,
    pub quote_ttl_ms: u64,
    pub core_package: ObjectID,
    pub trading_vault_package: ObjectID,
    /// Shared `VaultProtocolConfig` (token-info `trading_vault_objects`).
    /// Needed to create a vault and to deposit into one.
    pub vault_protocol_config: ObjectID,
    /// `[testnet]` faucet seed for a vault this bot provisions. `Some`
    /// only on testnet with `mint_and_deposit_liquidity = true`.
    pub testnet_seed: Option<provision::TestnetSeed>,
    /// options_adapter package — vault-funded auction bids disabled
    /// when absent.
    pub options_adapter_package: Option<ObjectID>,
    /// deepbook_adapter package — vault-custody resale disabled when
    /// absent.
    pub deepbook_adapter_package: Option<ObjectID>,
    /// Shared `IntegrationRegistry` / `PoolAllowlist` (token-info
    /// `trading_vault_objects`). Curator-session flows need both.
    pub integration_registry: Option<ObjectID>,
    pub pool_allowlist: Option<ObjectID>,
    /// DeepBook deployment — resale/flash exits disabled when absent.
    pub deepbook: Option<DeepBookHandles>,
    /// The deployment's DEEP token type (token-info `deep_coin_type`).
    pub deep_coin_type: Option<String>,
}

/// Resolved on-chain identities for curator-session PTBs (vault-funded
/// bids + vault-custody exits). The `CuratorCap` is owned by the bot
/// wallet; callers refresh its object ref per tx (each submit bumps the
/// owned object's version).
#[derive(Clone, Copy, Debug)]
pub struct CuratorRefs {
    pub trading_vault_package: ObjectID,
    pub vault_id: ObjectID,
    pub curator_cap: ObjectID,
    pub integration_registry: ObjectID,
}

/// Boot the desk: reconstruct the book from vault custody, then spawn the
/// refresher, hedge rebalancers, auction bidder, exits and monitors.
/// Returns the handle the WS serve loop prices through.
pub async fn spawn_desk(p: DeskParams) -> Result<Arc<Desk>> {
    // Reconstruct the book from vault custody.
    let wrap = sui_tx::sui_client::SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let indexer = indexer_graphql::IndexerClient::new(p.indexer_url.clone());
    let api = api_service_client::ApiServiceClient::new(&p.api_url);

    // Adopt a pinned / self-created vault, or provision one. Chain state
    // decides usability — a vault the config merely claims is fine still
    // fails here rather than degrading into a desk that declines
    // everything (SO-345).
    let resolved = provision::resolve(provision::ResolveParams {
        wrap: &wrap,
        indexer: &indexer,
        cfg: &p.cfg.provision,
        pinned_vault_id: p.cfg.vault_id.trim(),
        allow_mm_release_toggle: p.cfg.mm_release_enabled,
        trading_vault_package: p.trading_vault_package,
        vault_protocol_config: p.vault_protocol_config,
        settlement_coin_type: &p.settlement_coin_type,
        testnet_seed: p.testnet_seed.as_ref(),
    })
    .await
    .inspect_err(provision::report_unusable)?;
    let vault_id = resolved.vault_id;
    let vault_address = SuiAddress::from_bytes(vault_id.into_bytes())
        .map_err(|e| anyhow!("vault id → address: {e}"))?;

    // Per-market models.
    let surface: SurfaceConfig = p.cfg.surface.into();
    let models: Arc<Vec<MarketModel>> = Arc::new(
        p.markets
            .iter()
            .map(|m| {
                MarketModel::new(
                    m.symbol.clone(),
                    m.coin_type.clone(),
                    Arc::clone(&m.vol_buf),
                    Arc::clone(&m.vol_buf_long),
                    m.fallback_vol,
                    p.cfg.carry_yields.get(&m.symbol).copied().unwrap_or(0.0),
                    p.rate,
                    surface,
                )
            })
            .collect(),
    );
    let market_feeds: Vec<(PriceFeedId, u8)> =
        p.markets.iter().map(|m| (m.feed, m.decimals)).collect();

    let book = book::reconstruct(book::ReconstructParams {
        wrap: &wrap,
        indexer: &indexer,
        api: &api,
        trading_vault_package: p.trading_vault_package,
        vault_id,
        settlement_coin_type: p.settlement_coin_type.clone(),
        pnl_path: Some(std::path::PathBuf::from(&p.cfg.pnl_jsonl_path)),
    })
    .await
    .context("reconstructing the desk book from vault custody")?;
    let book = Arc::new(RwLock::new(book));

    // Curator refs for vault-funded flows (bids escrowed from vault
    // balances, vault-custody exits). `resolve` already proved this wallet
    // owns the cap with a chain read; the registry comes from token-info.
    // Missing either disables those flows with a warning — wallet-side
    // exits and WS quoting still run.
    let curator_refs = match (resolved.curator_cap, p.integration_registry) {
        (Some(curator_cap), Some(integration_registry)) => Some(CuratorRefs {
            trading_vault_package: p.trading_vault_package,
            vault_id,
            curator_cap,
            integration_registry,
        }),
        (cap, reg) => {
            tracing::warn!(
                curator_cap_found = cap.is_some(),
                integration_registry_found = reg.is_some(),
                "curator refs unresolved — vault-funded bids and vault-custody exits disabled"
            );
            None
        }
    };

    // Hedge venue roster: `[[desk.hedge.venues]]`, or the compat default
    // of one paper venue from the legacy `paper_*` knobs. Every spec is
    // instantiated per underlying; the FIRST spec is the primary
    // (execution) venue the rebalancer trades — the rest are monitored
    // (position/margin/funding feed the aggregates).
    let venue_specs = p.cfg.hedge.venue_specs()?;
    let primary_spec = venue_specs[0].clone();

    let shared = Arc::new(DeskShared {
        exposure: RwLock::new(BookExposure::default()),
        book_delta_units: RwLock::new(HashMap::new()),
        naked_written_units: RwLock::new(0),
        funding_rate_annual: RwLock::new(primary_spec.funding_rate_annual),
        stress_blocked: AtomicBool::new(false),
        expected_holding_years: p.cfg.expected_holding_years,
        slippage_bps: primary_spec.slippage_bps,
        marks: RwLock::new(HashMap::new()),
        spots: RwLock::new(HashMap::new()),
        stress: RwLock::new(None),
    });

    let mut hedge_venues: Vec<Arc<dyn hedge::HedgeVenue>> = Vec::new();
    let mut monitor_venues: Vec<monitors::MonitorVenue> = Vec::new();
    // Per-market PRIMARY venue, aligned with `p.markets`.
    let mut primary_venues: Vec<Arc<dyn hedge::HedgeVenue>> = Vec::new();
    for m in &p.markets {
        for (vi, spec) in venue_specs.iter().enumerate() {
            // The "paper"-named venue keeps the legacy per-symbol state
            // filename so existing state survives the multi-venue change.
            let file = if spec.name == "paper" {
                format!("paper-hedge-{}.json", m.symbol.to_lowercase())
            } else {
                format!("paper-hedge-{}-{}.json", spec.name, m.symbol.to_lowercase())
            };
            let venue: Arc<dyn hedge::HedgeVenue> = Arc::new(hedge::PaperVenue::load_named(
                spec.name.clone(),
                std::path::PathBuf::from(&p.cfg.state_dir).join(file),
                spec.slippage_bps,
                spec.funding_rate_annual,
            ));
            if vi == 0 {
                primary_venues.push(Arc::clone(&venue));
            }
            monitor_venues.push(monitors::MonitorVenue {
                symbol: m.symbol.clone(),
                venue: Arc::clone(&venue),
            });
            hedge_venues.push(venue);
        }
    }

    // Book refresher: marks, greeks, NAV, custody re-sync, kill switch,
    // P&L accrual.
    spawn_book_refresher(RefresherParams {
        cfg: p.cfg.clone(),
        wrap,
        indexer,
        api,
        vault_id,
        trading_vault_package: p.trading_vault_package,
        book: Arc::clone(&book),
        shared: Arc::clone(&shared),
        models: Arc::clone(&models),
        market_feeds: market_feeds.clone(),
        price_cache: p.price_cache.clone(),
        settlement_feed: p.settlement_feed,
        settlement_decimals: p.settlement_decimals,
        staleness: p.staleness,
    });

    // Fill detection → spread-line P&L attribution, resumed from the
    // persisted events-feed cursor.
    spawn_fill_poller(FillPollerParams {
        cfg: p.cfg.clone(),
        indexer: indexer_graphql::IndexerClient::new(p.indexer_url.clone()),
        api: api_service_client::ApiServiceClient::new(&p.api_url),
        book: Arc::clone(&book),
        models: Arc::clone(&models),
        market_feeds: market_feeds.clone(),
        price_cache: p.price_cache.clone(),
        settlement_feed: p.settlement_feed,
        settlement_decimals: p.settlement_decimals,
        staleness: p.staleness,
        vault_id: protocol_types::ids::ObjectId::new(vault_id.into_bytes()),
    });

    // Hedge rebalancers (bands, not clocks) — primary venue per market.
    for (i, m) in p.markets.iter().enumerate() {
        spawn_rebalancer(RebalancerParams {
            hedge_cfg: p.cfg.hedge.clone(),
            venue: Arc::clone(&primary_venues[i]),
            shared: Arc::clone(&shared),
            book: Arc::clone(&book),
            coin_type: m.coin_type.clone(),
            symbol: m.symbol.clone(),
            feed: m.feed,
            decimals: m.decimals,
            price_cache: p.price_cache.clone(),
            settlement_feed: p.settlement_feed,
            settlement_decimals: p.settlement_decimals,
            staleness: p.staleness,
        });
    }

    // On-chain auction channel: bids escrow from VAULT balances via
    // `options_adapter::bid_on_auction` (BidTicket custody).
    if p.cfg.auctions.enabled {
        match (p.options_adapter_package, curator_refs) {
            (Some(options_adapter_package), Some(curator)) => {
                auctions::spawn_bidder(auctions::AuctionBidderParams {
                    cfg: p.cfg.auctions.clone(),
                    v1: p.cfg.v1.into(),
                    limits: p.cfg.limits,
                    shared: Arc::clone(&shared),
                    secrets: p.secrets.clone(),
                    network: p.network,
                    options_adapter_package,
                    curator,
                    book: Arc::clone(&book),
                    api_url: p.api_url.clone(),
                    indexer_url: p.indexer_url.clone(),
                    price_cache: p.price_cache.clone(),
                    models: Arc::clone(&models),
                    settlement_feed: p.settlement_feed,
                    settlement_coin_type: p.settlement_coin_type.clone(),
                    settlement_decimals: p.settlement_decimals,
                    market_feeds: market_feeds.clone(),
                    staleness: p.staleness,
                    expected_holding_years: p.cfg.expected_holding_years,
                    slippage_bps: primary_spec.slippage_bps,
                })
            }
            _ => tracing::warn!(
                "[desk.auctions] enabled but options_adapter package or curator refs missing; \
                 channel off"
            ),
        }
    }

    // Exit ladder.
    exits::spawn_exits(exits::ExitsParams {
        cfg: p.cfg.exits.clone(),
        secrets: p.secrets.clone(),
        network: p.network,
        book: Arc::clone(&book),
        models: Arc::clone(&models),
        market_feeds: market_feeds.clone(),
        price_cache: p.price_cache.clone(),
        settlement_feed: p.settlement_feed,
        settlement_coin_type: p.settlement_coin_type.clone(),
        settlement_decimals: p.settlement_decimals,
        staleness: p.staleness,
        handles: p.deepbook,
        deep_coin_type: p.deep_coin_type.clone(),
        core_package: p.core_package,
        vault_address,
        curator: curator_refs,
        deepbook_adapter_package: p.deepbook_adapter_package,
        pool_allowlist: p.pool_allowlist,
    });

    // `/desk/state` reads the same roster the monitors watch.
    let venue_roster: Vec<monitors::MonitorVenue> = monitor_venues
        .iter()
        .map(|v| monitors::MonitorVenue { symbol: v.symbol.clone(), venue: Arc::clone(&v.venue) })
        .collect();

    // Monitors + nightly stress over the WHOLE venue roster: summed
    // shorts per underlying for the delta band, min margin headroom
    // (alerts name the venue), notional-weighted funding into pricing.
    if !monitor_venues.is_empty() {
        monitors::spawn_monitors(monitors::MonitorsParams {
            cfg: p.cfg.monitors,
            limits: p.cfg.limits,
            shared: Arc::clone(&shared),
            book: Arc::clone(&book),
            models: Arc::clone(&models),
            market_feeds: market_feeds.clone(),
            price_cache: p.price_cache.clone(),
            settlement_feed: p.settlement_feed,
            settlement_decimals: p.settlement_decimals,
            staleness: p.staleness,
            venues: monitor_venues,
            hedge_band_pct_nav: p.cfg.hedge.band_pct_nav,
        });
    }

    tracing::info!(
        vault = %vault_id,
        provisioned = resolved.provisioned,
        curator_cap = resolved.curator_cap.is_some(),
        markets = p.markets.len(),
        hedge_venues = venue_specs.len(),
        v2 = p.cfg.v2.enabled,
        "desk started (vault-only maker)"
    );
    let market_meta = p
        .markets
        .iter()
        .map(|m| state::MarketMeta {
            symbol: m.symbol.clone(),
            coin_type: m.coin_type.clone(),
            decimals: m.decimals,
            fallback_vol: m.fallback_vol,
        })
        .collect();
    Ok(Arc::new(Desk {
        v1: p.cfg.v1.into(),
        v2: p.cfg.v2.enabled.then(|| p.cfg.v2.into()),
        limits: p.cfg.limits,
        quote_ttl_ms: p.quote_ttl_ms,
        cfg: p.cfg,
        vault_id,
        shared,
        book,
        models,
        hedge_venues,
        venue_roster,
        provisioned: resolved.provisioned,
        curator_refs,
        booted_at_ms: auctions::now_ms(),
        market_meta,
        settlement_coin_type: p.settlement_coin_type.clone(),
        settlement_decimals: p.settlement_decimals,
    }))
}

// ── book refresher ─────────────────────────────────────────────────────

struct RefresherParams {
    cfg: DeskConfig,
    wrap: sui_tx::sui_client::SuiClientWrapper,
    indexer: indexer_graphql::IndexerClient,
    api: api_service_client::ApiServiceClient,
    vault_id: ObjectID,
    trading_vault_package: ObjectID,
    book: Arc<RwLock<Book>>,
    shared: Arc<DeskShared>,
    models: Arc<Vec<MarketModel>>,
    market_feeds: Vec<(PriceFeedId, u8)>,
    price_cache: PriceCache,
    settlement_feed: PriceFeedId,
    settlement_decimals: u8,
    staleness: Staleness,
}

fn spawn_book_refresher(p: RefresherParams) {
    tokio::spawn(async move {
        let mut kill = limits::KillSwitch::load(
            std::path::PathBuf::from(&p.cfg.state_dir).join("desk-nav-history.json"),
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(p.cfg.refresh_secs.max(15)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_theta_accrual = auctions::now_ms();
        let mut tick_count: u64 = 0;
        loop {
            ticker.tick().await;
            let now = auctions::now_ms();
            tick_count += 1;

            // Custody re-sync every 5th tick (auction wins / keeper
            // sweeps / exits / new writes change custody out-of-band):
            // held coins AND written positions, then re-net `covered`.
            if tick_count % 5 == 1 {
                let holdings = book::fetch_holdings(
                    &p.wrap,
                    &p.indexer,
                    &p.api,
                    p.trading_vault_package,
                    p.vault_id,
                )
                .await;
                let written =
                    book::fetch_written(&p.wrap, &p.indexer, &p.api, p.vault_id).await;
                let mut b = p.book.write();
                match holdings {
                    Ok(h) => b.holdings = h,
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "custody re-sync failed; keeping holdings")
                    }
                }
                match written {
                    Ok(w) => b.written = w,
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "written re-sync failed; keeping written lines")
                    }
                }
                b.recompute_covered();
            }

            // NAV from the indexer view (pps × shares; see book docs).
            let nav = match p.indexer.trading_vaults().await {
                Ok(vaults) => vaults
                    .iter()
                    .find(|v| {
                        let hex = p.vault_id.to_hex_literal();
                        v.vault_id.to_hex() == hex || format!("0x{}", v.vault_id.to_hex()) == hex
                    })
                    .and_then(|v| {
                        v.latest_pps_e12.map(|pps| {
                            u64::try_from(pps.saturating_mul(v.total_shares) / 1_000_000_000_000u128)
                                .unwrap_or(u64::MAX)
                        })
                    }),
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "NAV refresh: indexer unreachable");
                    None
                }
            };

            // Fresh spot per model (holdings/written marks + `/desk/state`
            // share the same per-tick observation).
            let spot_by_model: Vec<Option<f64>> = (0..p.models.len())
                .map(|mi| {
                    let (feed, decimals) = p.market_feeds[mi];
                    compute_spot_from_cache(
                        &p.price_cache,
                        feed,
                        p.settlement_feed,
                        decimals,
                        p.settlement_decimals,
                        p.staleness,
                    )
                    .ok()
                })
                .collect();
            {
                let mut spots = p.shared.spots.write();
                for (mi, m) in p.models.iter().enumerate() {
                    if let Some(spot) = spot_by_model[mi] {
                        spots.insert(m.symbol.clone(), SpotSnapshot { spot, at_ms: now });
                    }
                }
            }

            // Marks + greeks per holding/written line.
            let (holdings, written) = {
                let b = p.book.read();
                (b.holdings.clone(), b.written.clone())
            };
            let mut exposure = BookExposure::default();
            let mut marks: HashMap<protocol_types::ids::ObjectId, MarkSnapshot> = HashMap::new();
            let mut delta_by_coin: HashMap<String, f64> = HashMap::new();
            let mut deployed = 0.0f64;
            for h in &holdings {
                let Some(mi) = p.models.iter().position(|m| m.coin_type == h.asset_coin_type)
                else {
                    continue;
                };
                let Some(spot) = spot_by_model[mi] else {
                    continue;
                };
                let t = h.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
                let k = h.strike_scaled();
                let (sigma, _) = p.models[mi].sigma(spot, k, t);
                let mark = p.models[mi].fair_per_unit(h.is_put, spot, k, t, sigma);
                let g = p.models[mi].greeks_per_unit(h.is_put, spot, k, t, sigma);
                marks.insert(
                    h.bucket_id.clone(),
                    MarkSnapshot { mark_per_unit: mark, sigma, spot, greeks: g, at_ms: now },
                );
                let amt = h.amount() as f64;
                deployed += mark * amt;
                exposure.net_vega_per_volpt += g.vega * amt / 100.0;
                exposure.theta_cost_per_day += (-g.theta * amt).max(0.0);
                *exposure.premium_by_expiry.entry(h.expiry_ms).or_default() += mark * amt;
                exposure.premium_by_strike_bucket[limits::strike_bucket(k, spot)] += mark * amt;
                *delta_by_coin.entry(h.asset_coin_type.clone()).or_default() += g.delta * amt;
            }
            // Written lines subtract their full greeks so quoting sees
            // TRUE nets (net vega = held − written, same for delta/
            // gamma/theta). A written bucket with no held coin still
            // needs per-unit marks computed here.
            for w in &written {
                let Some(mi) = p.models.iter().position(|m| m.coin_type == w.asset_coin_type)
                else {
                    continue;
                };
                let g = match marks.get(&w.bucket_id) {
                    Some(m) => m.greeks,
                    None => {
                        let Some(spot) = spot_by_model[mi] else {
                            continue;
                        };
                        let t =
                            w.expiry_ms.saturating_sub(now) as f64 / 1000.0 / 86_400.0 / 365.0;
                        let k = w.strike_scaled();
                        let (sigma, _) = p.models[mi].sigma(spot, k, t);
                        let mark = p.models[mi].fair_per_unit(w.is_put, spot, k, t, sigma);
                        let g = p.models[mi].greeks_per_unit(w.is_put, spot, k, t, sigma);
                        marks.insert(
                            w.bucket_id,
                            MarkSnapshot { mark_per_unit: mark, sigma, spot, greeks: g, at_ms: now },
                        );
                        g
                    }
                };
                let amt = w.amount as f64;
                exposure.net_vega_per_volpt -= g.vega * amt / 100.0;
                exposure.theta_cost_per_day -= (-g.theta * amt).max(0.0);
                *delta_by_coin.entry(w.asset_coin_type.clone()).or_default() -= g.delta * amt;
            }
            *p.shared.marks.write() = marks;

            // Theta accrual → P&L attribution.
            let dt_days = now.saturating_sub(last_theta_accrual) as f64 / 86_400_000.0;
            last_theta_accrual = now;
            {
                let mut b = p.book.write();
                if let Some(nav) = nav {
                    b.nav = nav;
                }
                b.deployed = deployed.max(0.0) as u64;
                b.expire_reservations(now);
                if dt_days > 0.0 && exposure.theta_cost_per_day > 0.0 {
                    b.record_pnl(
                        PnlLine::Theta,
                        -exposure.theta_cost_per_day * dt_days,
                        "accrual",
                        now,
                    );
                }
                exposure.nav = b.nav as f64;
                exposure.reserved = b.reserved_total() as f64;
                exposure.premium_deployed = b.deployed as f64;
                *p.shared.naked_written_units.write() = b.naked_written_units();
            }
            exposure.kill_switch = kill.check(&p.cfg.limits, exposure.nav as u64, now);
            *p.shared.book_delta_units.write() = delta_by_coin;
            *p.shared.exposure.write() = exposure;
        }
    });
}

// ── fill poller (spread-line P&L attribution) ──────────────────────────

struct FillPollerParams {
    cfg: DeskConfig,
    indexer: indexer_graphql::IndexerClient,
    api: api_service_client::ApiServiceClient,
    book: Arc<RwLock<Book>>,
    models: Arc<Vec<MarketModel>>,
    market_feeds: Vec<(PriceFeedId, u8)>,
    price_cache: PriceCache,
    settlement_feed: PriceFeedId,
    settlement_decimals: u8,
    staleness: Staleness,
    vault_id: protocol_types::ids::ObjectId,
}

/// How a detected fill priced this tick.
enum FairOutcome {
    Total(f64),
    /// Permanently unpriceable (bucket unknown / not a served market):
    /// skip the fill with a zero-spread warn and advance past it.
    Skip,
    /// Transient (api error / stale spot): stop the batch, retry next
    /// tick — the cursor stays put so nothing is lost.
    Retry,
}

/// Poll the indexer events feed since the persisted cursor and attribute
/// each detected fill to the spread line (see `book::apply_fills` for
/// the documented fair-at-detection approximation). First boot seeds the
/// cursor at the indexer head so pre-desk history is never replayed as
/// fills; afterwards the cursor only advances write-after-apply.
fn spawn_fill_poller(p: FillPollerParams) {
    tokio::spawn(async move {
        let cursor_path =
            std::path::PathBuf::from(&p.cfg.state_dir).join("desk-fill-cursor.json");
        let mut cursor = book::FillCursor::load(&cursor_path);
        let mut ticker = tokio::time::interval(Duration::from_secs(p.cfg.refresh_secs.max(15)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Newest events fetched per query, per tick. Fills between polls
        // beyond this bound would be dropped — far beyond any realistic
        // per-minute fill rate on this protocol.
        const MAX_EVENTS: usize = 500;
        loop {
            ticker.tick().await;
            let mut cur = match cursor {
                Some(c) => c,
                None => match p.indexer.head_sequence().await {
                    Ok(head) => {
                        let c = book::FillCursor { last_sequence: head };
                        c.persist(&cursor_path);
                        tracing::info!(head, "fill cursor seeded at indexer head");
                        cursor = Some(c);
                        c
                    }
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "fill cursor seed failed; retrying");
                        continue;
                    }
                },
            };

            // Our fills: WS-RFQ writes released from the vault's
            // collateral + auction-channel wins observed as ticket
            // redemptions (see the identity note in `book`).
            let vault_hex = p.vault_id.to_hex();
            let queries: [(&[&str], serde_json::Value); 2] = [
                (&["WriteExecuted"], serde_json::json!({ "collateral_source": vault_hex })),
                (&["PutWriteExecuted"], serde_json::json!({ "collateral_source": vault_hex })),
            ];
            let mut fills: Vec<book::DetectedFill> = Vec::new();
            let mut feed_ok = true;
            for (types, fields) in queries {
                match p.indexer.recent_events_with_payload(types, fields, MAX_EVENTS).await {
                    Ok(events) => fills.extend(
                        events
                            .iter()
                            .filter(|ev| ev.sequence > cur.last_sequence)
                            .filter_map(|ev| book::classify_fill(ev, p.vault_id)),
                    ),
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "fill poll failed; retrying next tick");
                        feed_ok = false;
                        break;
                    }
                }
            }
            // Auction wins: TvBidRedeemed ⋈ TvBidPlaced (by ticket).
            // Placed events are fetched WITHOUT the cursor filter — the
            // placement always precedes the redemption we attribute.
            if feed_ok {
                let vault_filter = serde_json::json!({ "vault_id": vault_hex });
                let placed = p
                    .indexer
                    .recent_events_with_payload(&["TvBidPlaced"], vault_filter.clone(), MAX_EVENTS)
                    .await;
                let redeemed = p
                    .indexer
                    .recent_events_with_payload(&["TvBidRedeemed"], vault_filter, MAX_EVENTS)
                    .await;
                match (placed, redeemed) {
                    (Ok(placed), Ok(redeemed)) => {
                        let placed_by_ticket: HashMap<
                            protocol_types::ids::ObjectId,
                            protocol_types::events::TvBidPlaced,
                        > = placed
                            .iter()
                            .filter_map(|ev| match &ev.event {
                                protocol_types::events::ChainEvent::TvBidPlaced(b) => {
                                    Some((b.ticket_id, b.clone()))
                                }
                                _ => None,
                            })
                            .collect();
                        fills.extend(
                            redeemed
                                .iter()
                                .filter(|ev| ev.sequence > cur.last_sequence)
                                .filter_map(|ev| {
                                    book::classify_ticket_win(ev, p.vault_id, &placed_by_ticket)
                                }),
                        );
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        tracing::debug!(error = %format!("{e:#}"), "ticket-win poll failed; retrying next tick");
                        feed_ok = false;
                    }
                }
            }
            if !feed_ok || fills.is_empty() {
                continue;
            }
            fills.sort_by_key(|f| f.sequence);

            // Price each fill at the CURRENT surface, in order; stop at
            // the first transient failure so the cursor never jumps a
            // fill that could still be priced.
            let now = auctions::now_ms();
            let mut priced: Vec<(book::DetectedFill, f64)> = Vec::new();
            for f in fills {
                match fill_fair_total(&p, &f, now).await {
                    FairOutcome::Total(fair) => priced.push((f, fair)),
                    FairOutcome::Skip => {
                        tracing::warn!(
                            seq = f.sequence,
                            bucket = %f.bucket_id.to_hex(),
                            "fill unpriceable (bucket/market unknown); spread 0 recorded"
                        );
                        // fair == premium ⇒ zero spread, cursor advances.
                        let fair = f.premium as f64;
                        priced.push((f, fair));
                    }
                    FairOutcome::Retry => break,
                }
            }
            if priced.is_empty() {
                continue;
            }
            let applied = {
                let mut b = p.book.write();
                book::apply_fills(&mut b, &mut cur, &cursor_path, &priced, now)
            };
            cursor = Some(cur);
            if applied > 0 {
                tracing::info!(applied, cursor = cur.last_sequence, "fills attributed to spread line");
            }
        }
    });
}

/// Model fair TOTAL premium for a fill at the current surface.
async fn fill_fair_total(
    p: &FillPollerParams,
    f: &book::DetectedFill,
    now_ms: u64,
) -> FairOutcome {
    let bucket = match p.api.bucket_pricing(f.bucket_id).await {
        Ok(Some(b)) => b,
        Ok(None) => return FairOutcome::Skip,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "bucket lookup failed for fill");
            return FairOutcome::Retry;
        }
    };
    let Some(mi) = p.models.iter().position(|m| m.coin_type == bucket.asset_coin_type) else {
        return FairOutcome::Skip;
    };
    let (feed, decimals) = p.market_feeds[mi];
    let Ok(spot) = compute_spot_from_cache(
        &p.price_cache,
        feed,
        p.settlement_feed,
        decimals,
        p.settlement_decimals,
        p.staleness,
    ) else {
        return FairOutcome::Retry;
    };
    let t = bucket.expiry_ms.saturating_sub(now_ms) as f64 / 1000.0 / 86_400.0 / 365.0;
    let k = bucket.strike as f64 / 10f64.powi(bucket.strike_scale as i32);
    let (sigma, _) = p.models[mi].sigma(spot, k, t);
    FairOutcome::Total(p.models[mi].fair_per_unit(bucket.is_put, spot, k, t, sigma) * f.amount as f64)
}

// ── hedge rebalancer ───────────────────────────────────────────────────

struct RebalancerParams {
    hedge_cfg: hedge::HedgeConfig,
    venue: Arc<dyn hedge::HedgeVenue>,
    shared: Arc<DeskShared>,
    book: Arc<RwLock<Book>>,
    coin_type: String,
    symbol: String,
    feed: PriceFeedId,
    decimals: u8,
    price_cache: PriceCache,
    settlement_feed: PriceFeedId,
    settlement_decimals: u8,
    staleness: Staleness,
}

fn spawn_rebalancer(p: RebalancerParams) {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(p.hedge_cfg.interval_secs.max(5)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Seed from the venue so a restart doesn't re-attribute the whole
        // persisted realized P&L as fresh scalp.
        let mut last_realized = p.venue.realized_pnl().await.unwrap_or(0.0);
        loop {
            ticker.tick().await;
            // The venue's own funding drives THIS band decision; the
            // aggregate (notional-weighted across venues) that pricing
            // consumes is written by the monitors.
            let funding = match p.venue.funding_rate_annual().await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "funding read failed");
                    continue;
                }
            };
            let Ok(spot) = compute_spot_from_cache(
                &p.price_cache,
                p.feed,
                p.settlement_feed,
                p.decimals,
                p.settlement_decimals,
                p.staleness,
            ) else {
                continue;
            };
            let nav = p.shared.exposure.read().nav;
            let delta = p
                .shared
                .book_delta_units
                .read()
                .get(&p.coin_type)
                .copied()
                .unwrap_or(0.0);
            let short = match p.venue.position_units().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "hedge position read failed");
                    continue;
                }
            };
            let band = hedge::band_units(&p.hedge_cfg, nav, spot, funding);
            if let Some(target) = hedge::rebalance_target(delta, short, band) {
                if let Err(e) = p.venue.adjust_to(target, spot).await {
                    tracing::error!(
                        alert_id = "tx-failed-mm-bot-desk",
                        venue = p.venue.name(),
                        symbol = %p.symbol,
                        error = %format!("{e:#}"),
                        "hedge adjust failed"
                    );
                    continue;
                }
                // Long-gamma rebalancing sells high / buys low: realized
                // hedge P&L is the scalp line.
                let realized = p.venue.realized_pnl().await.unwrap_or(last_realized);
                let scalp = realized - last_realized;
                last_realized = realized;
                if scalp != 0.0 {
                    p.book
                        .write()
                        .record_pnl(PnlLine::Scalp, scalp, "hedge rebalance", auctions::now_ms());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_single_venue_desk_toml_still_parses() {
        // A pre-multi-venue `[desk]` section: `[desk.hedge]` scalar knobs
        // only, no `[[desk.hedge.venues]]` array.
        let cfg: DeskConfig = toml::from_str(
            "enabled = true\n\
             vault_id = \"0x1\"\n\
             mm_release_enabled = true\n\
             [hedge]\n\
             band_pct_nav = 2.0\n\
             paper_slippage_bps = 3.0\n",
        )
        .unwrap();
        assert!(cfg.enabled);
        let specs = cfg.hedge.venue_specs().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "paper");
        assert!((specs[0].slippage_bps - 3.0).abs() < 1e-12);
    }

    #[test]
    fn desk_toml_with_venue_array_parses() {
        let cfg: DeskConfig = toml::from_str(
            "vault_id = \"0x1\"\n\
             [[hedge.venues]]\n\
             kind = \"paper\"\n\
             [[hedge.venues]]\n\
             kind = \"paper\"\n\
             name = \"paper-b\"\n",
        )
        .unwrap();
        let specs = cfg.hedge.venue_specs().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].name, "paper-b");
    }
}
