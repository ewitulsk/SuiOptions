//! The vol desk (SO-299): delta-hedged long-vol fund. Long-only — the
//! desk never writes options (SO-426, doc 08 §4.1); trader-flow RFQs
//! decline unconditionally. Replaces every legacy strategy module.
//!
//! Standing product decision (doc 05): the bot trades ONLY as the trading
//! vault's curator — quotes route collateral from the vault
//! (`release_module = "vault_mm"`, outputs to the vault address), auction
//! winnings land in the vault, exits pay the vault. `spawn_desk` resolves
//! that vault through [`provision`] — adopting a pinned or self-created
//! one, or creating it — and refuses to start without a usable vault.

pub mod auctions;
pub mod book;
pub mod exchange_client;
pub mod exits;
pub mod guards;
pub mod hedge;
pub mod history;
pub mod limits;
pub mod listings;
pub mod monitors;
pub mod positions;
pub mod provision;
pub mod rfq;
pub mod state;

// Pure policy modules re-exported from the strategy kernel (SO-450) so
// every `desk::model` / `desk::quote` path keeps resolving.
pub use desk_core::exposure::{MarkSnapshot, SpotSnapshot};
pub use desk_core::{model, quote};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sui_types::base_types::{ObjectID, SuiAddress};

use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::Network;
use sui_tx::tx::deepbook::DeepBookHandles;

use crate::pricing::{compute_spot_from_cache, Staleness};

use book::{Book, PnlLine};
use limits::{BookExposure, LimitsConfig};
use model::{EstimatorConfig, EstimatorKind, MarketModel, SurfaceConfig, V1BidParams};
use quote::{Decision, FlowContext, RfqInputs};
use vol_forecast::{PriceHistory, RollingVolBuffer};

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
    /// `[desk.capital]` — freshness gates, liquidity reserve, and the
    /// venue/flash capacity assumptions behind the capital snapshot
    /// (doc 08 §0.4/§4.6, SO-444).
    pub capital: limits::CapitalConfig,
    pub v1: V1Config,
    pub hedge: hedge::HedgeConfig,
    pub auctions: auctions::AuctionsConfig,
    pub exits: exits::ExitsConfig,
    /// `[desk.listings]` — resting-ask exits on the in-house exchange
    /// (SO-416).
    pub listings: listings::ListingsConfig,
    pub monitors: monitors::MonitorsConfig,
    /// `[desk.provision]` — create a vault when there is none to adopt.
    pub provision: provision::ProvisionConfig,
    /// `[desk.history]` — TimescaleDB time-series recorder (SO-349).
    pub history: history::HistoryConfig,
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
            capital: limits::CapitalConfig::default(),
            v1: V1Config::default(),
            hedge: hedge::HedgeConfig::default(),
            auctions: auctions::AuctionsConfig::default(),
            exits: exits::ExitsConfig::default(),
            listings: listings::ListingsConfig::default(),
            monitors: monitors::MonitorsConfig::default(),
            provision: provision::ProvisionConfig::default(),
            history: history::HistoryConfig::default(),
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
    /// ATM sigma source (SO-440): `"windows"` (two-window blend, default)
    /// or `"har"` (vol-forecast HAR-RV at `q_bid`). The forecast runs in
    /// shadow either way and shows on `/desk/state`.
    pub estimator: EstimatorKind,
    /// Forecast quantile the bid prices at when `estimator = "har"`.
    pub q_bid: f64,
    /// HAR calibration refit cadence.
    pub refit_secs: u64,
    /// Derive wing convexity from the asset's own kurtosis (`"har"` only).
    pub convexity_from_kurtosis: bool,
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
            estimator: EstimatorKind::Windows,
            q_bid: 0.35,
            refit_secs: 86_400,
            convexity_from_kurtosis: false,
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
    /// Fraction of expected funding INCOME credited into the bid
    /// (doc 08 §4.3). 0 = conservative: income is upside, never priced.
    pub funding_income_credit: f64,
    /// Composition soft throttle (doc 08 §4.5, SO-445): bid widening at
    /// a hard composition threshold, vol points. Provisional 0.05.
    pub composition_penalty_volpts: f64,
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
            funding_income_credit: 0.0,
            composition_penalty_volpts: 0.05,
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
            funding_income_credit: c.funding_income_credit,
            composition_penalty_volpts: c.composition_penalty_volpts,
        }
    }
}

// ── shared runtime state ───────────────────────────────────────────────

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
    /// Naked written units across the book — legacy written-inventory
    /// detection only (SO-426): the desk never writes options, so any
    /// nonzero value is pre-existing inventory that blocks new quotes
    /// until unwound.
    pub naked_written_units: RwLock<u64>,
    /// Last observed hedge funding rate (annualized).
    pub funding_rate_annual: RwLock<f64>,
    /// Nightly stress gate: block new short risk (V2 §7).
    pub stress_blocked: AtomicBool,
    /// SO-418 capital risk gate: true while the desk's vault is risk-off
    /// (risk state not Healthy, curator commitment breached, lifecycle
    /// not open, or settled). Quote sessions and `vault_mm` releases
    /// abort on-chain in these states (code 124), so quoting, auction
    /// bids and new listings hard-stop here BEFORE burning gas. Seeded at
    /// boot from the resolved vault; refreshed from the indexer view
    /// every book-refresher tick.
    pub risk_off: AtomicBool,
    pub expected_holding_years: f64,
    /// Bid-side venue cost inputs (primary venue slippage + `[desk.hedge]`
    /// fee/turnover/margin knobs) — SO-437.
    pub hedge_cost: pricing::desk::HedgeCostParams,
    /// Last signed hedge position per underlying coin type on the primary
    /// venue, hedge units; the rebalancer writes, the bid reads (SO-437).
    pub hedge_position_units: RwLock<HashMap<String, f64>>,
    /// Per-bucket marks + per-unit greeks from the last refresher tick
    /// (`/desk/state` reads; the refresher writes).
    pub marks: RwLock<HashMap<protocol_types::ids::ObjectId, MarkSnapshot>>,
    /// Per-symbol spot from the last refresher tick.
    pub spots: RwLock<HashMap<String, SpotSnapshot>>,
    /// Last stress-suite result (per-scenario drawdowns + the gate).
    pub stress: RwLock<Option<StressSnapshot>>,
    /// Per-bucket resting exchange asks (the listings engine writes;
    /// `/desk/state` reads) — SO-416.
    pub listings: RwLock<HashMap<protocol_types::ids::ObjectId, listings::ListingSnapshot>>,
    /// Venue margin picture from the monitors' last roster read (the
    /// capital snapshot's input) — SO-444.
    pub venue_margin: RwLock<limits::VenueMarginInputs>,
}

impl DeskShared {
    pub async fn flow_context(&self, spot: f64, coin_type: &str) -> FlowContext {
        FlowContext {
            spot,
            exposure: self.exposure.read().clone(),
            funding_rate_annual: *self.funding_rate_annual.read(),
            expected_holding_years: self.expected_holding_years,
            hedge_position_units: self
                .hedge_position_units
                .read()
                .get(coin_type)
                .copied()
                .unwrap_or(0.0),
            hedge_cost: self.hedge_cost,
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
    /// Desk history DB — the durable side of the reservation ledger
    /// (SO-444). `None` ⇒ reservations are process-local only.
    history: Option<Arc<history::History>>,
    v1: V1BidParams,
    limits: LimitsConfig,
    quote_ttl_ms: u64,
}

/// What a signed WS quote reserves under (SO-444): the service request
/// id keys the reservation; the quote nonce joins it to the chain fill.
#[derive(Clone, Debug)]
pub struct QuoteKey {
    pub request_id: String,
    pub nonce: u64,
}

impl Desk {
    /// Flush every reservation transition queued in the book to the
    /// durable ledger (no-op without a history DB).
    fn persist_reservations(&self) {
        let transitions = self.book.write().drain_reservation_transitions();
        if let Some(h) = &self.history {
            h.record_reservations(transitions);
        }
    }

    /// The signed quote never reached the taker (send failure): free its
    /// reservation now rather than at TTL.
    pub fn revert_quote(&self, request_id: &str, now_ms: u64) {
        if self.book.write().revert_reservation(request_id, now_ms) {
            self.persist_reservations();
        }
    }

    /// The model's vol for one market at a point on the surface — the input
    /// the moneyness guard sizes its band from, so the band widens and
    /// narrows with the same surface the pricing uses.
    pub fn model_sigma(&self, model_index: usize, spot: f64, strike: f64, t_years: f64) -> f64 {
        self.models[model_index].sigma(spot, strike, t_years).0
    }

    /// Price one WS RFQ. `Side::Writer` = retail writes (the desk buys).
    /// `Side::Trader` = retail buys — the desk NEVER writes options
    /// (SO-426, doc 08 §4.1), so trader RFQs always decline. With
    /// `reserve`, a writer-flow quote reserves its premium under the
    /// request id for the quote TTL plus the fill-detection grace, and
    /// the reservation is persisted (pass `None` for indicative bulk
    /// views — nothing is signed there).
    pub async fn price_ws_rfq(
        &self,
        side: Side,
        model_index: usize,
        inputs: RfqInputs,
        spot: f64,
        reserve: Option<QuoteKey>,
        now_ms: u64,
    ) -> Decision {
        // SO-418 risk gate: every signed quote routes collateral through
        // `vault_mm::release`, which aborts (code 124) whenever the vault
        // is risk-off — decline before pricing, reserving, or signing.
        if self.shared.risk_off.load(Ordering::Relaxed) {
            return Decision::Decline {
                reason: "vault risk-off (capital risk state / commitment breach)".into(),
            };
        }
        // Legacy written inventory is a migration problem, not a
        // strategy: surface it and block new quoting until it is
        // unwound (doc 08 §4.1 gate).
        let naked = *self.shared.naked_written_units.read();
        if naked > 0 {
            return Decision::Decline {
                reason: format!(
                    "legacy written inventory present ({naked} naked units); quoting blocked until unwound"
                ),
            };
        }
        let model = &self.models[model_index];
        let ctx = self.shared.flow_context(spot, &model.coin_type).await;
        match side {
            Side::Writer => {
                let d = quote::price_writer_flow(model, &self.v1, &self.limits, &ctx, &inputs, now_ms);
                if let (
                    Some(key),
                    Decision::Quote { premium, hedge_notional, exercise_cash, .. },
                ) = (reserve, &d)
                {
                    // Reserve the premium while the quote is live (plus
                    // the fill-detection grace); a detected fill, a
                    // revert, or TTL expiry closes it.
                    let ttl_ms = self
                        .quote_ttl_ms
                        .saturating_add(self.cfg.capital.reservation_grace_secs.saturating_mul(1000));
                    let res = self.book.write().reserve_quote(
                        book::QuoteReservation {
                            key: key.request_id,
                            nonce: Some(key.nonce),
                            amount: *premium,
                            is_put: inputs.is_put,
                            expiry_ms: inputs.expiry_ms,
                            exercise_cash: *exercise_cash,
                            hedge_notional: *hedge_notional,
                            ttl_ms,
                        },
                        now_ms,
                    );
                    match res {
                        Ok(()) => self.persist_reservations(),
                        Err(book::ReserveError::ExceedsNav) => {
                            return Decision::Decline {
                                reason: "reservation ledger full (reservations + deployed ≥ NAV)"
                                    .into(),
                            };
                        }
                        Err(book::ReserveError::DuplicateKey) => {
                            return Decision::Decline {
                                reason: "duplicate request id: a live reservation already holds it"
                                    .into(),
                            };
                        }
                    }
                }
                d
            }
            Side::Trader => Decision::Decline {
                reason: "desk does not write options (long-only strategy)".into(),
            },
        }
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
    /// Long price history the vol forecaster reads (SO-440).
    pub history: Arc<RwLock<PriceHistory>>,
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
    /// Shared `whitelist::Whitelist` — the ingress gate on `create_vault`
    /// / `vault::deposit` (SO-383).
    pub whitelist: ObjectID,
    /// `[testnet]` faucet seed for a vault this bot provisions. `Some`
    /// only on testnet with `mint_and_deposit_liquidity = true`.
    pub testnet_seed: Option<provision::TestnetSeed>,
    /// options_adapter package — vault-funded auction bids disabled
    /// when absent.
    pub options_adapter_package: Option<ObjectID>,
    /// exchange_adapter package — exchange listings (direct escrow)
    /// disabled when absent.
    pub exchange_adapter_package: Option<ObjectID>,
    /// exchange_listing package + shared `ListingAuthority` —
    /// permissionless option-market listing (SO-416); with them absent
    /// the listings engine only serves markets that already exist.
    pub exchange_listing_package: Option<ObjectID>,
    pub exchange_listing_authority: Option<ObjectID>,
    /// Shared `IntegrationRegistry` (token-info `trading_vault_objects`).
    /// Curator-session flows need it.
    pub integration_registry: Option<ObjectID>,
    /// DeepBook deployment — flash-exercise exits disabled when absent.
    pub deepbook: Option<DeepBookHandles>,
    /// The deployment's DEEP token type (token-info `deep_coin_type`).
    pub deep_coin_type: Option<String>,
    /// deepbook-adapter package + shared `PoolAllowlist` — the
    /// vault-custody put repurchase (SO-443) is disabled when absent.
    pub deepbook_adapter: Option<exits::put::AdapterRefs>,
    /// Desk history DB — the durable reservation ledger (SO-444) and,
    /// behind `[desk.history] record_rfq_outcomes`, the RFQ funnel the
    /// fill poller closes rows in (SO-425). `None` ⇒ neither, trading
    /// unaffected.
    pub history: Option<Arc<history::History>>,
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
        whitelist: p.whitelist,
        settlement_coin_type: &p.settlement_coin_type,
        testnet_seed: p.testnet_seed.as_ref(),
    })
    .await
    .inspect_err(provision::report_unusable)?;
    let vault_id = resolved.vault_id;
    let vault_address = SuiAddress::from_bytes(vault_id.into_bytes())
        .map_err(|e| anyhow!("vault id → address: {e}"))?;

    // SO-418 position custody: v2 deposits mint `VaultPosition` NFTs into
    // this wallet (the testnet seed, commitment releases). Merge them to
    // one per (tranche, generation) so the owned-object count stays
    // bounded across restarts. Best-effort — a failure never gates boot.
    if let Err(e) = positions::merge_owned_positions(
        &wrap,
        p.trading_vault_package,
        vault_id,
        p.cfg.provision.gas_budget,
    )
    .await
    {
        tracing::warn!(error = %format!("{e:#}"), "vault-position merge pass failed; continuing");
    }

    // Per-market models.
    let surface: SurfaceConfig = p.cfg.surface.into();
    let estimator = EstimatorConfig {
        kind: p.cfg.surface.estimator,
        q_bid: p.cfg.surface.q_bid,
        refit_secs: p.cfg.surface.refit_secs,
        horizon_ms: (p.cfg.expected_holding_years * vol_forecast::MS_PER_YEAR).round() as u64,
        convexity_from_kurtosis: p.cfg.surface.convexity_from_kurtosis,
    };
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
                .with_estimator(Arc::clone(&m.history), estimator)
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
        options_package: Some(p.core_package.to_hex_literal()),
    })
    .await
    .context("reconstructing the desk book from vault custody")?;
    let book = Arc::new(RwLock::new(book));

    // Durable reservations (SO-444): re-install every still-live quote
    // reservation from the history DB, reconciled against the chain
    // fills the vault's collateral released while we were down.
    if let Some(h) = &p.history {
        let loader = Arc::clone(h);
        let rows = tokio::task::spawn_blocking(move || loader.load_live_reservations()).await;
        match rows.map_err(anyhow::Error::from) {
            Ok(Ok(rows)) => {
                let vault_pt = protocol_types::ids::ObjectId::new(vault_id.into_bytes());
                let filled = match recent_fill_nonces(&indexer, vault_pt).await {
                    Ok(set) => set,
                    Err(e) => {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "fill scan for reservation reconciliation failed; keeping rows live (TTL closes them)"
                        );
                        std::collections::HashSet::new()
                    }
                };
                let now = auctions::now_ms();
                let (live, transitions) = book::reconcile_reservations(rows, &filled, now);
                let mut b = book.write();
                let restored = live.len();
                for r in live {
                    b.restore_reservation(r);
                }
                tracing::info!(
                    restored,
                    closed = transitions.len(),
                    reserved = b.reserved_total(),
                    "quote reservations reconstructed from durable state + chain fills"
                );
                h.record_reservations(transitions);
            }
            Ok(Err(e)) | Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "durable reservation load failed; starting with an empty ledger"
            ),
        }
    }

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
        risk_off: AtomicBool::new(resolved.risk_off),
        expected_holding_years: p.cfg.expected_holding_years,
        hedge_cost: p.cfg.hedge.cost_params(primary_spec.slippage_bps),
        hedge_position_units: RwLock::new(HashMap::new()),
        marks: RwLock::new(HashMap::new()),
        spots: RwLock::new(HashMap::new()),
        stress: RwLock::new(None),
        listings: RwLock::new(HashMap::new()),
        venue_margin: RwLock::new(limits::VenueMarginInputs::default()),
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
            let venue: Arc<dyn hedge::HedgeVenue> = Arc::new(
                hedge::PaperVenue::load_named(
                    spec.name.clone(),
                    std::path::PathBuf::from(&p.cfg.state_dir).join(file),
                    spec.slippage_bps,
                    spec.funding_rate_annual,
                )
                .with_fill_fraction(spec.fill_fraction),
            );
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
        options_package: Some(p.core_package.to_hex_literal()),
        settlement_coin_type: p.settlement_coin_type.clone(),
        history: p.history.clone(),
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
        history: p.history.clone(),
        rfq: p.history.as_ref().and_then(|h| h.rfq_recorder()),
    });

    // Hedge rebalancers (bands, not clocks) — primary venue per market.
    for (i, m) in p.markets.iter().enumerate() {
        spawn_rebalancer(RebalancerParams {
            hedge_cfg: p.cfg.hedge.clone(),
            pnl_path: std::path::PathBuf::from(&p.cfg.pnl_jsonl_path),
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

    // RETIRED on-chain auction channel (bids escrowed from VAULT
    // balances via `options_adapter::bid_on_auction`): the auction venue
    // is deprecated and the entry function no longer exists in fresh
    // deployments, so this stays off unless explicitly re-enabled
    // against a pre-retirement deployment.
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

    // Exit ladder (offset close / hold / exercise; resale is the
    // listings engine's resting asks).
    exits::spawn_exits(exits::ExitsParams {
        cfg: p.cfg.exits.clone(),
        secrets: p.secrets.clone(),
        network: p.network,
        shared: Arc::clone(&shared),
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
        deepbook_adapter: p.deepbook_adapter,
        hedge_venues: primary_venues.clone(),
    });

    // Exchange listings (SO-416): rest asks for the desk's option
    // inventory on the in-house exchange; the exchange's matching engine
    // executes the resales.
    listings::spawn_listings(listings::ListingsParams {
        cfg: p.cfg.listings.clone(),
        refresh_secs: p.cfg.refresh_secs,
        secrets: p.secrets.clone(),
        network: p.network,
        book: Arc::clone(&book),
        shared: Arc::clone(&shared),
        curator: curator_refs,
        vault_protocol_config: p.vault_protocol_config,
        exchange_adapter_package: p.exchange_adapter_package,
        exchange_listing_package: p.exchange_listing_package,
        exchange_listing_authority: p.exchange_listing_authority,
        indexer_url: p.indexer_url.clone(),
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
            initial_margin_fraction: p.cfg.hedge.initial_margin_fraction,
            maintenance_margin_fraction: p.cfg.capital.maintenance_margin_fraction,
        });
    }

    tracing::info!(
        vault = %vault_id,
        provisioned = resolved.provisioned,
        curator_cap = resolved.curator_cap.is_some(),
        markets = p.markets.len(),
        hedge_venues = venue_specs.len(),
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
        history: p.history,
        v1: p.cfg.v1.into(),
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
    options_package: Option<String>,
    settlement_coin_type: String,
    /// Durable reservation ledger sink (TTL expiries) — SO-444.
    history: Option<Arc<history::History>>,
}

/// The nonces of every recent chain fill the vault's collateral released
/// (the boot reconciliation input; same feed the fill poller scans).
async fn recent_fill_nonces(
    indexer: &indexer_graphql::IndexerClient,
    vault: protocol_types::ids::ObjectId,
) -> Result<std::collections::HashSet<u64>> {
    const MAX_EVENTS: usize = 500;
    let mut out = std::collections::HashSet::new();
    let filter = serde_json::json!({ "collateral_source": vault.to_hex() });
    for ty in ["WriteExecuted", "PutWriteExecuted"] {
        let events = indexer
            .recent_events_with_payload(&[ty], filter.clone(), MAX_EVENTS)
            .await
            .with_context(|| format!("scanning {ty} fills"))?;
        out.extend(events.iter().filter_map(|ev| match book::classify_fill(ev, vault)?.link {
            book::FillLink::WsQuote { nonce } => Some(nonce),
            book::FillLink::AuctionTicket { .. } => None,
        }));
    }
    Ok(out)
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
        // Last successful chain reads for the capital snapshot: a
        // transient RPC failure reuses them rather than reporting an
        // empty vault (which would collapse local NAV and free cash).
        let mut last_free: Option<(f64, HashMap<String, f64>)> = None;
        let mut last_external_limits: Option<(u64, u64, u64, u64)> = None;
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
                    p.options_package.as_deref(),
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

            // Budget base + risk state from the indexer view (SO-418:
            // latest/junior NAV, not total pps × shares — see
            // `book::budget_base`). The same view feeds the capital
            // snapshot (appraisal freshness, external account, queued
            // withdrawals — SO-444).
            let mut vault_view: Option<indexer_graphql::TradingVault> = None;
            let nav = match p.indexer.trading_vaults().await {
                Ok(vaults) => {
                    let ours = vaults.iter().find(|v| {
                        let hex = p.vault_id.to_hex_literal();
                        v.vault_id.to_hex() == hex || format!("0x{}", v.vault_id.to_hex()) == hex
                    });
                    vault_view = ours.cloned();
                    if let Some(v) = ours {
                        // Risk gate refresh, with transition logging: the
                        // desk keeps running (healthy-but-idle) either way.
                        let now_off = book::vault_risk_off(v);
                        let was_off = p.shared.risk_off.swap(now_off, Ordering::Relaxed);
                        if now_off && !was_off {
                            tracing::warn!(
                                vault = %p.vault_id,
                                risk_state = v.risk_state,
                                commitment_breached = v.curator_commitment_breached,
                                state = %v.state,
                                settled = v.settled,
                                "desk vault went RISK-OFF — quoting, bids and new listings stop"
                            );
                        } else if !now_off && was_off {
                            tracing::info!(vault = %p.vault_id, "desk vault risk state cured — resuming");
                        }
                        metrics::gauge!("mm_desk_vault_risk_off").set(if now_off { 1.0 } else { 0.0 });
                    }
                    ours.and_then(book::budget_base)
                }
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

            // Marks + greeks per holding/written line: the kernel's mark
            // pass (`desk_core::exposure`, SO-450) over the book.
            let pass = {
                let b = p.book.read();
                desk_core::exposure::mark_book(desk_core::exposure::MarkInputs {
                    models: &p.models,
                    holdings: &b.holdings,
                    written: &b.written,
                    spot_by_model: &spot_by_model,
                    now_ms: now,
                    stress_gap_down: p.cfg.monitors.stress_gap_down.abs(),
                    stress_gap_up: p.cfg.monitors.stress_gap_up.abs(),
                    quote_flash_capacity: p.cfg.capital.quote_flash_capacity,
                    base_flash_capacity: p.cfg.capital.base_flash_capacity,
                })
            };
            let desk_core::exposure::MarkPass {
                mut exposure,
                marks,
                delta_by_coin,
                deployed,
                call_strike_cash,
                put_underlying_value,
                exercise_demand_by_expiry,
                hedge_notional,
                hedge_notional_by_expiry,
            } = pass;
            *p.shared.marks.write() = marks;

            // Capital snapshot inputs read from chain (SO-444): the
            // vault's free settlement + free underlying (valued at spot)
            // and, with an external account registered, its on-chain
            // release limits.
            let free = {
                let settlement = book::free_balance_of(
                    &p.wrap,
                    p.trading_vault_package,
                    p.vault_id,
                    &p.settlement_coin_type,
                )
                .await;
                match settlement {
                    Ok(s) => {
                        let mut underlying = HashMap::new();
                        for (mi, m) in p.models.iter().enumerate() {
                            let Some(spot) = spot_by_model[mi] else { continue };
                            match book::free_balance_of(
                                &p.wrap,
                                p.trading_vault_package,
                                p.vault_id,
                                &m.coin_type,
                            )
                            .await
                            {
                                Ok(units) => {
                                    underlying.insert(m.symbol.clone(), units as f64 * spot);
                                }
                                Err(e) => tracing::debug!(
                                    error = %format!("{e:#}"),
                                    symbol = %m.symbol,
                                    "free underlying read failed; counted as 0 this tick"
                                ),
                            }
                        }
                        last_free = Some((s as f64, underlying));
                    }
                    Err(e) => tracing::debug!(
                        error = %format!("{e:#}"),
                        "free settlement read failed; reusing the last reading"
                    ),
                }
                last_free.clone().unwrap_or_default()
            };
            let external = match &vault_view {
                Some(v) if v.external_account.is_some() => {
                    match book::external_limits(&p.wrap, p.trading_vault_package, p.vault_id).await {
                        Ok(l) => last_external_limits = Some(l),
                        Err(e) => tracing::debug!(
                            error = %format!("{e:#}"),
                            "external_limits read failed; reusing the last reading"
                        ),
                    }
                    let (budget_bps, daily_release_bps, released_in_window, window_start_ms) =
                        last_external_limits.unwrap_or_default();
                    Some(limits::ExternalInputs {
                        exposure: v.external_exposure as f64,
                        equity: v.latest_external_equity.map(|e| e as f64),
                        equity_at: v.external_equity_updated_at_ms,
                        budget_bps,
                        daily_release_bps,
                        released_in_window: released_in_window as f64,
                        window_start_ms,
                        nav_for_limits: v.latest_nav.map(|n| n as f64),
                    })
                }
                _ => None,
            };
            // Queued withdrawals valued at the tranche's observed pps;
            // `None` (unvaluable) declines new quotes.
            let queued_withdrawal_value = match &vault_view {
                Some(v) => {
                    let vault_pt = protocol_types::ids::ObjectId::new(p.vault_id.into_bytes());
                    match p.indexer.vault_positions(vault_pt).await {
                        Ok(positions) => queued_withdrawal_value(v, &positions),
                        Err(e) => {
                            tracing::debug!(error = %format!("{e:#}"), "vault_positions read failed");
                            None
                        }
                    }
                }
                None => None,
            };

            // Theta accrual → P&L attribution.
            let dt_days = now.saturating_sub(last_theta_accrual) as f64 / 86_400_000.0;
            last_theta_accrual = now;
            let transitions = {
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
                    book::flush_pnl(
                        &mut b,
                        Some(std::path::Path::new(&p.cfg.pnl_jsonl_path)),
                    );
                }
                exposure.nav = b.nav as f64;
                exposure.reserved = b.reserved_total() as f64;
                exposure.premium_deployed = b.deployed as f64;
                *p.shared.naked_written_units.write() = b.naked_written_units();
                let reserved = b.reserved_split();
                exposure.capital = limits::build_capital_snapshot(
                    &p.cfg.capital,
                    limits::CapitalInputs {
                        now_ms: now,
                        appraised_nav: nav.map(|n| n as f64),
                        appraisal_at: vault_view.as_ref().and_then(|v| v.nav_updated_at_ms),
                        free_settlement: free.0,
                        free_underlying_by_asset: free.1,
                        premium_deployed: exposure.premium_deployed,
                        call_premium_marked: exposure.call_premium,
                        put_premium_marked: exposure.put_premium,
                        premium_by_expiry_marked: &exposure.premium_by_expiry,
                        call_strike_cash_marked: call_strike_cash,
                        put_underlying_value_marked: put_underlying_value,
                        exercise_demand_by_expiry_marked: &exercise_demand_by_expiry,
                        hedge_notional_marked: hedge_notional,
                        hedge_notional_by_expiry_marked: &hedge_notional_by_expiry,
                        reserved: &reserved,
                        queued_withdrawal_value,
                        external,
                        venue: *p.shared.venue_margin.read(),
                        initial_margin_fraction: p.cfg.hedge.initial_margin_fraction,
                        stress_gap: p.cfg.monitors.stress_gap_down.abs().max(p.cfg.monitors.stress_gap_up.abs()),
                    },
                );
                b.drain_reservation_transitions()
            };
            if let Some(h) = &p.history {
                h.record_reservations(transitions);
            }
            if let Some(r) = exposure.capital.risk_nav {
                metrics::gauge!("mm_desk_risk_nav").set(r);
            }
            metrics::gauge!("mm_desk_capital_stale")
                .set(if exposure.capital.stale.is_empty() { 0.0 } else { 1.0 });
            exposure.kill_switch = kill.check(&p.cfg.limits, exposure.nav as u64, now);
            *p.shared.book_delta_units.write() = delta_by_coin;
            *p.shared.exposure.write() = exposure;
        }
    });
}

/// Value of every `queued` VaultPosition at its tranche's observed pps
/// (settlement raw); `None` when any queued position has no pps to value
/// it with. Mirrors `book::budget_base`'s pps scaling.
fn queued_withdrawal_value(
    v: &indexer_graphql::TradingVault,
    positions: &[indexer_graphql::VaultPosition],
) -> Option<f64> {
    const OFFSET: f64 = 1_000_000.0; // SHARE_OFFSET
    let mut total = 0.0;
    for pos in positions.iter().filter(|p| p.status == "queued") {
        let pps = match pos.tranche {
            1 => v.latest_senior_pps_e12,
            2 => v.latest_junior_pps_e12,
            _ => v.latest_pps_e12,
        }?;
        total += pos.shares as f64 * pps as f64 / 1e12 / OFFSET;
    }
    Some(total)
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
    history: Option<Arc<history::History>>,
    /// RFQ-funnel sink (SO-425), `None` unless the recorder flag is on.
    rfq: Option<Arc<dyn history::RfqRecorder>>,
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
            let (applied, transitions) = {
                let mut b = p.book.write();
                let applied = book::apply_fills(
                    &mut b,
                    &mut cur,
                    &cursor_path,
                    Some(std::path::Path::new(&p.cfg.pnl_jsonl_path)),
                    &priced,
                    now,
                );
                // A chain fill closes the quote's reservation (SO-444):
                // ground truth, idempotent when it is already closed.
                for (f, _) in &priced {
                    if let book::FillLink::WsQuote { nonce } = &f.link {
                        b.fill_reservation_by_nonce(*nonce, now);
                    }
                }
                (applied, b.drain_reservation_transitions())
            };
            cursor = Some(cur);
            if let Some(h) = &p.history {
                h.record_reservations(transitions);
            }
            close_filled_rfqs(&p.rfq, &priced, now);
            if applied > 0 {
                tracing::info!(applied, cursor = cur.last_sequence, "fills attributed to spread line");
            }
        }
    });
}

/// Close the RFQ-funnel rows these fills belong to (SO-425): the chain
/// fill's join key (quote nonce / auction ticket) upgrades the row to
/// `filled`. Idempotent updates — a replayed batch re-marks the same
/// rows filled. No-op without a sink.
pub(crate) fn close_filled_rfqs(
    rfq: &Option<Arc<dyn history::RfqRecorder>>,
    priced: &[(book::DetectedFill, f64)],
    now: u64,
) {
    let Some(r) = rfq else { return };
    for (f, _) in priced {
        let key = match &f.link {
            book::FillLink::WsQuote { nonce } => history::RfqFillKey::Nonce(*nonce),
            book::FillLink::AuctionTicket { ticket } => {
                history::RfqFillKey::Request(ticket.to_hex())
            }
        };
        r.record_rfq_filled(key, f.sequence, now);
    }
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
    /// P&L attribution JSONL sink (scalp + funding lines).
    pnl_path: std::path::PathBuf,
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

/// One market's rebalancer: its dependencies plus the state one tick
/// hands the next (SO-438 working orders, the scalp high-water mark).
/// The loop body is [`Rebalancer::rebalance_once`] so the runtime path
/// is testable tick by tick (SO-447).
pub(crate) struct Rebalancer {
    hedge_cfg: hedge::HedgeConfig,
    venue: Arc<dyn hedge::HedgeVenue>,
    shared: Arc<DeskShared>,
    book: Arc<RwLock<Book>>,
    coin_type: String,
    symbol: String,
    /// Venue realized P&L already attributed to the scalp line.
    last_realized: f64,
    /// Process-local order ids (unique per run; the funnel/event log
    /// carries the venue name alongside).
    next_order_id: hedge::OrderId,
    /// Working orders (SO-438): partial fills and late fills arrive
    /// through `poll_events`; unfilled size counts toward the net so a
    /// slow fill is never re-submitted, and stale orders are cancelled.
    open: hedge::OpenOrders,
    /// P&L attribution JSONL sink (scalp + funding lines); `None` in
    /// tests.
    pnl_path: Option<std::path::PathBuf>,
}

impl Rebalancer {
    async fn new(
        hedge_cfg: hedge::HedgeConfig,
        venue: Arc<dyn hedge::HedgeVenue>,
        shared: Arc<DeskShared>,
        book: Arc<RwLock<Book>>,
        coin_type: String,
        symbol: String,
        pnl_path: Option<std::path::PathBuf>,
    ) -> Self {
        // Seed from the venue so a restart doesn't re-attribute the whole
        // persisted realized P&L as fresh scalp.
        let last_realized = venue.realized_pnl().await.unwrap_or(0.0);
        Self {
            hedge_cfg,
            venue,
            shared,
            book,
            coin_type,
            symbol,
            last_realized,
            next_order_id: 0,
            open: hedge::OpenOrders::default(),
            pnl_path,
        }
    }

    /// One tick. `spot` is `None` when no fresh spot exists this tick:
    /// working orders are still reconciled (events drained, stale ones
    /// cancelled) but nothing new is planned.
    pub(crate) async fn rebalance_once(&mut self, now: u64, spot: Option<f64>) {
        let timeout_ms = self.hedge_cfg.order_timeout_secs.max(1) * 1000;
        match self.venue.poll_events().await {
            Ok(events) => {
                for ev in &events {
                    self.open.apply(ev);
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "hedge event poll failed"),
        }
        for id in self.open.stale(now, timeout_ms) {
            match self.venue.execute(hedge::HedgeCommand::Cancel(id)).await {
                Ok(events) => {
                    for ev in &events {
                        self.open.apply(ev);
                    }
                    tracing::warn!(venue = self.venue.name(), symbol = %self.symbol, order = id, "stale hedge order cancelled");
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), order = id, "hedge cancel failed"),
            }
        }
        // The venue's own funding drives THIS band decision; the
        // aggregate (notional-weighted across venues) that pricing
        // consumes is written by the monitors.
        let funding = match self.venue.funding_rate_annual().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "funding read failed");
                return;
            }
        };
        let Some(spot) = spot else {
            return;
        };
        let nav = self.shared.exposure.read().nav;
        let delta = self
            .shared
            .book_delta_units
            .read()
            .get(&self.coin_type)
            .copied()
            .unwrap_or(0.0);
        let position = match self.venue.position_units().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "hedge position read failed");
                return;
            }
        };
        self.shared.hedge_position_units.write().insert(self.coin_type.clone(), position);
        // Funding accrues on the signed position every tick, as its
        // own P&L line — never through the fills-only realized figure.
        match self.venue.accrue_funding(now, spot).await {
            Ok(paid) if paid != 0.0 => {
                let mut b = self.book.write();
                b.record_pnl(PnlLine::Funding, -paid, "hedge funding accrual", now);
                book::flush_pnl(&mut b, self.pnl_path.as_deref());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "hedge funding accrual failed"),
        }
        let band = hedge::band_units(&self.hedge_cfg, nav, spot, funding);
        if let Some(size) = hedge::plan_hedge_order(delta, position, self.open.working_units(), band) {
            self.next_order_id += 1;
            let order = hedge::HedgeOrder { id: self.next_order_id, size_units: size, spot };
            self.open.submit(&order, now);
            match self.venue.execute(hedge::HedgeCommand::Submit(order)).await {
                Ok(events) => {
                    for ev in &events {
                        self.open.apply(ev);
                        if let hedge::HedgeEvent::Rejected { order, reason } = ev {
                            tracing::error!(
                                alert_id = "tx-failed-mm-bot-desk",
                                venue = self.venue.name(),
                                symbol = %self.symbol,
                                order,
                                %reason,
                                "hedge order rejected"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        alert_id = "tx-failed-mm-bot-desk",
                        venue = self.venue.name(),
                        symbol = %self.symbol,
                        error = %format!("{e:#}"),
                        "hedge order failed"
                    );
                    return;
                }
            }
            // Long-gamma rebalancing sells high / buys low: realized
            // hedge P&L is the scalp line.
            let realized = self.venue.realized_pnl().await.unwrap_or(self.last_realized);
            let scalp = realized - self.last_realized;
            self.last_realized = realized;
            if scalp != 0.0 {
                let mut b = self.book.write();
                b.record_pnl(PnlLine::Scalp, scalp, "hedge rebalance", auctions::now_ms());
                book::flush_pnl(&mut b, self.pnl_path.as_deref());
            }
        }
    }
}

fn spawn_rebalancer(p: RebalancerParams) {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(p.hedge_cfg.interval_secs.max(5)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut r = Rebalancer::new(
            p.hedge_cfg,
            p.venue,
            p.shared,
            p.book,
            p.coin_type,
            p.symbol,
            Some(p.pnl_path),
        )
        .await;
        loop {
            ticker.tick().await;
            let spot = compute_spot_from_cache(
                &p.price_cache,
                p.feed,
                p.settlement_feed,
                p.decimals,
                p.settlement_decimals,
                p.staleness,
            )
            .ok();
            r.rebalance_once(auctions::now_ms(), spot).await;
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

    // ── SO-447 runtime paths: the desk's quote entry point ─────────────

    use hedge::HedgeVenue as _;
    use testkit::{COIN, DAY_MS};

    /// ATM call, 30 days out, 1M units.
    fn atm(is_put: bool) -> RfqInputs {
        RfqInputs { write_amount: 1_000_000, is_put, strike: 100, strike_scale: 0, expiry_ms: 30 * DAY_MS }
    }

    fn desk_with(shared: Arc<DeskShared>, venue: Arc<dyn hedge::HedgeVenue>) -> Desk {
        testkit::desk(shared, Arc::new(RwLock::new(Book::new(1_000_000_000))), venue)
    }

    /// Doc 08 §4.1 gate 1, through `Desk::price_ws_rfq`: a trader-side
    /// RFQ hard-declines with its stable reason and reserves nothing,
    /// while the same spec on the writer side quotes.
    #[tokio::test]
    async fn trader_side_rfq_hard_declines_through_the_desk() {
        let desk = desk_with(testkit::shared(1e9), testkit::paper_venue("trader", 0.0, 0.0, 1.0));
        let key = QuoteKey { request_id: "t1".into(), nonce: 1 };
        let d = desk.price_ws_rfq(Side::Trader, 0, atm(false), 100.0, Some(key), 0).await;
        assert_eq!(
            d,
            Decision::Decline { reason: "desk does not write options (long-only strategy)".into() }
        );
        assert_eq!(desk.book.read().reserved_total(), 0, "a decline must not reserve");
        let key = QuoteKey { request_id: "w1".into(), nonce: 2 };
        let d = desk.price_ws_rfq(Side::Writer, 0, atm(false), 100.0, Some(key), 0).await;
        assert!(matches!(d, Decision::Quote { .. }), "{d:?}");
        assert!(desk.book.read().reserved_total() > 0);
    }

    /// Doc 08 §4.1 gate 3: legacy written inventory the refresher lifts
    /// off the book blocks writer-side quoting until it is unwound.
    #[tokio::test]
    async fn legacy_naked_written_inventory_blocks_quoting() {
        let shared = testkit::shared(1e9);
        let desk = desk_with(Arc::clone(&shared), testkit::paper_venue("naked", 0.0, 0.0, 1.0));
        // A legacy short line on the book, as `book::fetch_written` would
        // reconstruct it; the refresher publishes its naked units.
        desk.book.write().written.push(book::Written {
            bucket_id: protocol_types::ids::ObjectId::new([1u8; 32]),
            position_id: protocol_types::ids::ObjectId::new([2u8; 32]),
            asset_coin_type: COIN.into(),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: 30 * DAY_MS,
            amount: 5,
            covered: 0,
        });
        *shared.naked_written_units.write() = desk.book.read().naked_written_units();
        let d = desk.price_ws_rfq(Side::Writer, 0, atm(false), 100.0, None, 0).await;
        assert_eq!(
            d,
            Decision::Decline {
                reason: "legacy written inventory present (5 naked units); quoting blocked until unwound"
                    .into()
            }
        );
        // Unwound ⇒ quoting resumes.
        desk.book.write().written.clear();
        *shared.naked_written_units.write() = desk.book.read().naked_written_units();
        let d = desk.price_ws_rfq(Side::Writer, 0, atm(false), 100.0, None, 0).await;
        assert!(matches!(d, Decision::Quote { .. }), "{d:?}");
    }

    // ── SO-447 runtime paths: the rebalancer ───────────────────────────

    /// NAV 1000 at spot 10 with the default 15% band ⇒ 15 units.
    const NAV: f64 = 1_000.0;
    const SPOT: f64 = 10.0;

    fn rebalancer(shared: &Arc<DeskShared>, venue: &Arc<hedge::PaperVenue>) -> Rebalancer {
        let venue: Arc<dyn hedge::HedgeVenue> = Arc::clone(venue) as Arc<dyn hedge::HedgeVenue>;
        Rebalancer {
            hedge_cfg: hedge::HedgeConfig::default(),
            venue,
            shared: Arc::clone(shared),
            book: Arc::new(RwLock::new(Book::new(NAV as u64))),
            coin_type: COIN.into(),
            symbol: "TSUI".into(),
            last_realized: 0.0,
            next_order_id: 0,
            open: hedge::OpenOrders::default(),
            pnl_path: None,
        }
    }

    fn set_delta(shared: &DeskShared, delta: f64) {
        shared.book_delta_units.write().insert(COIN.into(), delta);
    }

    fn published(shared: &DeskShared) -> Option<f64> {
        shared.hedge_position_units.read().get(COIN).copied()
    }

    /// Doc 08 §4.2 gate 1: a long-call book targets a SHORT perp. With a
    /// half-filling venue the remainder rides in `OpenOrders`, so the
    /// second tick does not resubmit it.
    #[tokio::test]
    async fn long_call_book_drives_a_short_perp_through_partial_fills() {
        let shared = testkit::shared(NAV);
        let venue = testkit::paper_venue("long-call", 0.0, 0.0, 0.5);
        let mut r = rebalancer(&shared, &venue);
        set_delta(&shared, 100.0);
        r.rebalance_once(1_000, Some(SPOT)).await;
        // Half filled synchronously; the other half is working.
        assert_eq!(venue.position_units().await.unwrap(), -50.0);
        assert_eq!(r.open.working_units(), -50.0);
        assert_eq!(r.next_order_id, 1);
        // The position published for pricing is the one read at tick
        // start (SO-437 reads it on the NEXT quote).
        assert_eq!(published(&shared), Some(0.0));
        // Counting the remainder, nothing more is due right now.
        assert_eq!(hedge::plan_hedge_order(100.0, -50.0, r.open.working_units(), 15.0), None);
        // Tick 2: the remainder fills on poll; no second order.
        r.rebalance_once(2_000, Some(SPOT)).await;
        assert_eq!(venue.position_units().await.unwrap(), -100.0);
        assert!(r.open.is_empty());
        assert_eq!(r.next_order_id, 1, "a working order must not be resubmitted");
        assert_eq!(published(&shared), Some(-100.0));
        // A stale spot reconciles working orders but plans nothing.
        set_delta(&shared, 200.0);
        r.rebalance_once(3_000, None).await;
        assert_eq!(r.next_order_id, 1);
    }

    /// Doc 08 §4.2 gate 2: a long-put book targets a LONG perp.
    #[tokio::test]
    async fn long_put_book_drives_a_long_perp() {
        let shared = testkit::shared(NAV);
        let venue = testkit::paper_venue("long-put", 0.0, 0.0, 0.5);
        let mut r = rebalancer(&shared, &venue);
        set_delta(&shared, -80.0);
        r.rebalance_once(1_000, Some(SPOT)).await;
        assert_eq!(venue.position_units().await.unwrap(), 40.0);
        r.rebalance_once(2_000, Some(SPOT)).await;
        assert_eq!(venue.position_units().await.unwrap(), 80.0);
        assert_eq!(r.next_order_id, 1);
        assert_eq!(published(&shared), Some(80.0));
    }

    /// Doc 08 §4.2 gate 3: equal and opposite deltas net to nothing and
    /// trade nothing; a residual nets before it trades.
    #[tokio::test]
    async fn mixed_book_nets_before_trading() {
        let shared = testkit::shared(NAV);
        let venue = testkit::paper_venue("mixed", 0.0, 0.0, 1.0);
        let mut r = rebalancer(&shared, &venue);
        // Calls +100, puts −100 (the refresher nets per underlying).
        set_delta(&shared, 100.0 + -100.0);
        r.rebalance_once(1_000, Some(SPOT)).await;
        assert_eq!(r.next_order_id, 0, "a netted book must not trade");
        assert_eq!(venue.position_units().await.unwrap(), 0.0);
        // Calls +100, puts −70: only the +30 residual is hedged.
        set_delta(&shared, 100.0 + -70.0);
        r.rebalance_once(2_000, Some(SPOT)).await;
        assert_eq!(r.next_order_id, 1);
        assert_eq!(venue.position_units().await.unwrap(), -30.0);
    }

    /// Doc 08 §4.2 gate 4: a direction reversal realizes the closed
    /// slice on the scalp line and re-opens the remainder.
    #[tokio::test]
    async fn direction_reversal_realizes_pnl_on_the_scalp_line() {
        let shared = testkit::shared(NAV);
        let venue = testkit::paper_venue("reversal", 0.0, 0.0, 1.0);
        let mut r = rebalancer(&shared, &venue);
        // Short 100 at 10 against a call book.
        set_delta(&shared, 100.0);
        r.rebalance_once(1_000, Some(SPOT)).await;
        assert_eq!(venue.position_units().await.unwrap(), -100.0);
        assert_eq!(r.book.read().pnl.scalp, 0.0);
        // The book flips put-heavy (−50) and spot drops to 8: buy 150 —
        // the 100 short closes at +2 × 100, the 50 long opens at 8.
        set_delta(&shared, -50.0);
        r.rebalance_once(2_000, Some(8.0)).await;
        assert_eq!(venue.position_units().await.unwrap(), 50.0);
        assert_eq!(venue.snapshot().await.avg_entry, 8.0);
        assert!((r.book.read().pnl.scalp - 200.0).abs() < 1e-9, "{}", r.book.read().pnl.scalp);
        // Back to call-heavy (+20) at 9: sell 70 — the 50 long closes at
        // +1 × 50, a 20 short opens.
        set_delta(&shared, 20.0);
        r.rebalance_once(3_000, Some(9.0)).await;
        assert_eq!(venue.position_units().await.unwrap(), -20.0);
        assert!((r.book.read().pnl.scalp - 250.0).abs() < 1e-9, "{}", r.book.read().pnl.scalp);
        assert_eq!(r.book.read().pnl.funding, 0.0, "no funding on a flat-rate venue");
    }

    /// Funding accrues on the signed perp position as `PnlLine::Funding`
    /// — a short RECEIVES positive funding — and never touches scalp.
    #[tokio::test]
    async fn funding_accrual_lands_on_the_funding_line_not_scalp() {
        let shared = testkit::shared(NAV);
        let venue = testkit::paper_venue("funding", 0.0, 0.10, 1.0);
        let mut r = rebalancer(&shared, &venue);
        set_delta(&shared, 100.0);
        // Tick 1 stamps the funding clock (flat) and opens the short.
        r.rebalance_once(1_000, Some(SPOT)).await;
        assert_eq!(venue.position_units().await.unwrap(), -100.0);
        assert_eq!(r.book.read().pnl.funding, 0.0);
        // One year later at +10%/yr on a 100-unit short at mark 10: the
        // short receives 0.10 × 100 × 10 = 100.
        let year = 365 * 86_400 * 1000;
        r.rebalance_once(1_000 + year, Some(SPOT)).await;
        let pnl = r.book.read().pnl;
        assert!((pnl.funding - 100.0).abs() < 1e-9, "{}", pnl.funding);
        assert_eq!(pnl.scalp, 0.0, "funding must never leak into scalp");
        assert_eq!(venue.realized_pnl().await.unwrap(), 0.0);
        assert!((venue.funding_paid().await.unwrap() + 100.0).abs() < 1e-9);
        assert_eq!(r.next_order_id, 1, "net delta is inside the band; nothing traded");
    }

    /// SO-437: the perp position the rebalancer publishes reaches the
    /// next quote via `flow_context.hedge_position_units`, so a put
    /// against a call-heavy (short-hedged) book prices as a reduction.
    #[tokio::test]
    async fn published_hedge_position_flows_into_the_next_quote() {
        let shared = testkit::shared(1e9);
        // Positive funding + margin financing make the hedge direction
        // matter: a fresh long-perp put hedge PAYS, a reducing one does
        // not.
        *shared.funding_rate_annual.write() = 0.30;
        let venue = testkit::paper_venue("position-aware", 0.0, 0.0, 1.0);
        let desk = desk_with(Arc::clone(&shared), Arc::clone(&venue) as Arc<dyn hedge::HedgeVenue>);
        async fn put_bid(desk: &Desk) -> u64 {
            match desk.price_ws_rfq(Side::Writer, 0, atm(true), 100.0, None, 0).await {
                Decision::Quote { premium, .. } => premium,
                other => panic!("expected Quote, got {other:?}"),
            }
        }
        let flat = put_bid(&desk).await;
        assert_eq!(shared.flow_context(100.0, COIN).await.hedge_position_units, 0.0);

        // A deeply call-heavy book: the rebalancer shorts 10M units at
        // spot 100 (band = 15% × 1e9 / 100 = 1.5M) and publishes it on
        // the following tick.
        let mut r = rebalancer(&shared, &venue);
        set_delta(&shared, 10_000_000.0);
        r.rebalance_once(1_000, Some(100.0)).await;
        r.rebalance_once(2_000, Some(100.0)).await;
        assert_eq!(venue.position_units().await.unwrap(), -10_000_000.0);
        assert_eq!(shared.flow_context(100.0, COIN).await.hedge_position_units, -10_000_000.0);

        let reducing = put_bid(&desk).await;
        assert!(reducing > flat, "reducing put bid {reducing} !> opening put bid {flat}");
    }
}

/// In-process desk fixtures for the runtime-path tests (SO-447): a real
/// `Desk`/`DeskShared` over an in-memory book and a paper venue, no
/// chain, indexer, or database.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    pub const DAY_MS: u64 = 86_400_000;
    pub const COIN: &str = "0x1::tsui::TSUI";
    pub const SETTLEMENT: &str = "0x1::tusdc::TUSDC";

    /// Model on cold vol buffers: the surface quotes the 0.60 fallback
    /// with no risk premium (same fixture as the `quote` tests).
    pub fn model() -> MarketModel {
        MarketModel::new(
            "TSUI".into(),
            COIN.into(),
            Arc::new(RwLock::new(RollingVolBuffer::new(DAY_MS))),
            Arc::new(RwLock::new(RollingVolBuffer::new(7 * DAY_MS))),
            0.60,
            0.0,
            0.0,
            SurfaceConfig {
                risk_premium: 0.0,
                skew: 0.0,
                convexity: 0.0,
                term_short_boost: 0.0,
                term_decay_years: 0.25,
                anchor_ratio: None,
                floor_vol: 0.01,
                cap_vol: 5.0,
                short_window_weight: 1.0,
                long_window_weight: 1.0,
            },
        )
    }

    /// Fresh, generously-funded shared state at `nav`; flat funding and
    /// zero venue costs so hedge-cost assertions are hand-checkable.
    pub fn shared(nav: f64) -> Arc<DeskShared> {
        Arc::new(DeskShared {
            exposure: RwLock::new(BookExposure {
                nav,
                capital: limits::CapitalSnapshot::test_fresh(nav, 0),
                ..Default::default()
            }),
            book_delta_units: RwLock::new(HashMap::new()),
            naked_written_units: RwLock::new(0),
            funding_rate_annual: RwLock::new(0.0),
            stress_blocked: AtomicBool::new(false),
            risk_off: AtomicBool::new(false),
            expected_holding_years: 21.0 / 365.0,
            hedge_cost: pricing::desk::HedgeCostParams {
                slippage_bps: 0.0,
                taker_fee_bps: 0.0,
                fixed_fee_per_fill: 0.0,
                rebalance_turnover_per_year: 0.0,
                margin_financing_rate_annual: 0.10,
                initial_margin_fraction: 0.10,
            },
            hedge_position_units: RwLock::new(HashMap::new()),
            marks: RwLock::new(HashMap::new()),
            spots: RwLock::new(HashMap::new()),
            stress: RwLock::new(None),
            listings: RwLock::new(HashMap::new()),
            venue_margin: RwLock::new(limits::VenueMarginInputs::default()),
        })
    }

    /// A paper venue on a fresh temp state file.
    pub fn paper_venue(
        tag: &str,
        slippage_bps: f64,
        funding_rate_annual: f64,
        fill_fraction: f64,
    ) -> Arc<hedge::PaperVenue> {
        let path = std::env::temp_dir().join(format!("so447-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Arc::new(
            hedge::PaperVenue::load(path, slippage_bps, funding_rate_annual)
                .with_fill_fraction(fill_fraction),
        )
    }

    /// A desk serving one market (`COIN`/`SETTLEMENT`) with no history
    /// DB and no curator refs.
    pub fn desk(
        shared: Arc<DeskShared>,
        book: Arc<RwLock<Book>>,
        venue: Arc<dyn hedge::HedgeVenue>,
    ) -> Desk {
        let cfg = DeskConfig::default();
        Desk {
            history: None,
            v1: cfg.v1.into(),
            limits: cfg.limits,
            quote_ttl_ms: 30_000,
            cfg,
            vault_id: ObjectID::ZERO,
            shared,
            book,
            models: Arc::new(vec![model()]),
            hedge_venues: vec![Arc::clone(&venue)],
            venue_roster: vec![monitors::MonitorVenue { symbol: "TSUI".into(), venue }],
            provisioned: false,
            curator_refs: None,
            booted_at_ms: 0,
            market_meta: vec![state::MarketMeta {
                symbol: "TSUI".into(),
                coin_type: COIN.into(),
                decimals: 9,
                fallback_vol: 0.60,
            }],
            settlement_coin_type: SETTLEMENT.into(),
            settlement_decimals: 6,
        }
    }
}
