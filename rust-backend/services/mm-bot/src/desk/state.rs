//! `/desk/state` snapshot (SO-348): a read-only JSON view of everything
//! the desk believes — vault wiring, exposure vs limits, per-expiry
//! greeks, holdings/written detail with model marks, the reservation
//! ledger, P&L attribution, and the hedge-venue roster.
//!
//! The snapshot never re-prices: marks/greeks/spots come from the book
//! refresher's last tick ([`super::DeskShared::marks`] /
//! [`super::DeskShared::spots`]), so serving it is lock-reads plus the
//! venue getters (in-memory for the paper venue). Field names are
//! camelCase; u128 strikes are decimal strings (the indexer convention);
//! u64 amounts stay JSON numbers.

use std::collections::HashMap;

use serde::Serialize;

use super::book::Reservation;
use super::hedge;
use super::limits::{
    Capacity, CapitalConfig, CapitalPolicy, CapitalSnapshot, FillRatios, LimitsConfig,
};
use super::model::Greeks;
use super::monitors::{read_venue, MonitorsConfig};
use super::{Desk, StressSnapshot, SurfaceTomlConfig, V1Config};

/// Static per-market metadata captured at desk boot (aligned with
/// `Desk::models`).
#[derive(Clone, Debug)]
pub struct MarketMeta {
    pub symbol: String,
    pub coin_type: String,
    pub decimals: u8,
    pub fallback_vol: f64,
}

// ── DTOs ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeskStateDto {
    pub generated_at_ms: u64,
    pub booted_at_ms: u64,
    pub network: String,
    pub vault: VaultDto,
    pub exposure: ExposureDto,
    pub utilization: UtilizationDto,
    pub limits: LimitsConfig,
    /// The capital snapshot every dollar cap derives from (doc 08 §4.6,
    /// SO-444) and the headline effective capacities over it.
    pub capital: CapitalSnapshot,
    pub capacities: CapacitiesDto,
    pub greeks: GreeksDto,
    /// Net book delta per underlying coin type, underlying raw units.
    pub book_delta_units: HashMap<String, f64>,
    pub naked_written_units: u64,
    /// Notional-weighted funding across venues (the pricing input).
    pub funding_rate_annual: f64,
    pub stress: Option<StressDto>,
    pub holdings: Vec<HoldingDto>,
    /// Resting exchange asks, one per listed holding (SO-416).
    pub listings: Vec<ListingDto>,
    pub written: Vec<WrittenDto>,
    pub reservations: ReservationsDto,
    pub pnl: PnlDto,
    pub hedge: HedgeDto,
    pub markets: Vec<MarketDto>,
    pub config: ConfigEchoDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultDto {
    pub vault_id: String,
    /// Whether this boot created the vault (vs adopting one).
    pub provisioned: bool,
    /// The CuratorCap object id when resolved. `None` disables
    /// vault-funded bids and vault-custody exits.
    pub curator_cap: Option<String>,
    /// Whether the IntegrationRegistry resolved (with the cap, gates the
    /// curator-session flows).
    pub curator_session_flows_enabled: bool,
    /// Operator attestation from config that `vault_mm` release is on.
    pub mm_release_enabled: bool,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureDto {
    /// Settlement raw units.
    pub nav: f64,
    pub premium_deployed: f64,
    pub reserved: f64,
    pub net_vega_per_volpt: f64,
    pub theta_cost_per_day: f64,
    /// expiry_ms → mark-to-model premium.
    pub premium_by_expiry: HashMap<u64, f64>,
    /// [<90%, 90–110%, >110%] moneyness buckets.
    pub premium_by_strike_bucket: [f64; 3],
    /// Composition surfaces (doc 08 §4.5, SO-431).
    pub call_premium: f64,
    pub put_premium: f64,
    pub delta_units_positive: f64,
    pub delta_units_negative: f64,
    pub gamma_units_calls: f64,
    pub gamma_units_puts: f64,
    pub kill_switch: bool,
    pub stress_blocked: bool,
    /// SO-418: the vault is risk-off (capital risk state / commitment
    /// breach / lifecycle) — quoting, bids and new listings are idle.
    pub risk_off: bool,
}

/// Continuous utilizations against the SOFT limits (1.0 = at limit),
/// mirroring `limits::evaluate`'s ratios with a zero proposed fill.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UtilizationDto {
    pub premium: f64,
    pub vega: f64,
    pub theta: f64,
}

/// Headline effective capacities (doc 08 §4.6) at the configured
/// reference ATM ratios; `None` while the snapshot cannot back new risk
/// (`stale` says why).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacitiesDto {
    pub risk_nav: Option<f64>,
    pub stale: Option<&'static str>,
    pub reference_ratios: ReferenceRatiosDto,
    pub effective_call_capacity: Option<Capacity>,
    pub effective_put_capacity: Option<Capacity>,
    /// Ascending by expiry, over the expiries the book/reservations hold.
    pub effective_expiry_capacity: Vec<ExpiryCapacityDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceRatiosDto {
    pub hedge_notional_per_premium: f64,
    pub exercise_cash_per_premium: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpiryCapacityDto {
    pub expiry_ms: u64,
    #[serde(flatten)]
    pub capacity: Capacity,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GreeksAggDto {
    pub delta_units: f64,
    pub gamma_units: f64,
    /// Premium change per 1.0 of vol (divide by 100 for per vol pt).
    pub vega: f64,
    /// Premium change per calendar day (negative = decay cost).
    pub theta_per_day: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpiryGreeksDto {
    pub expiry_ms: u64,
    #[serde(flatten)]
    pub greeks: GreeksAggDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GreeksDto {
    pub total: GreeksAggDto,
    /// Ascending by expiry.
    pub by_expiry: Vec<ExpiryGreeksDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StressDto {
    pub at_ms: u64,
    pub gap_down_60: f64,
    pub gap_up_80: f64,
    pub flat_6mo: f64,
    pub funding_minus_50: f64,
    pub worst_drawdown: f64,
    pub blocked: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GreeksPerUnitDto {
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
}

impl From<Greeks> for GreeksPerUnitDto {
    fn from(g: Greeks) -> Self {
        Self { delta: g.delta, gamma: g.gamma, vega: g.vega, theta: g.theta }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkDto {
    pub mark_per_unit: f64,
    /// Mark × line amount, settlement raw.
    pub value: f64,
    pub sigma: f64,
    pub spot: f64,
    pub at_ms: u64,
    pub greeks_per_unit: GreeksPerUnitDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoldingDto {
    pub bucket_id: String,
    pub option_coin_type: String,
    pub asset_coin_type: String,
    /// Underlying ticker when the coin type maps to a served market.
    pub symbol: Option<String>,
    pub is_put: bool,
    /// u128 as a decimal string.
    pub strike: String,
    pub strike_scale: u8,
    pub strike_scaled: f64,
    pub expiry_ms: u64,
    pub amount_vault: u64,
    pub amount_wallet: u64,
    pub amount_coin_positions: u64,
    pub amount: u64,
    /// Units committed to a resting exchange ask (SO-416).
    pub listed_units: u64,
    /// `None` when the market/spot was unavailable last tick.
    pub mark: Option<MarkDto>,
}

/// One holding's resting exchange ask (the listings engine, SO-416).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingDto {
    pub bucket_id: String,
    /// The exchange market's `SettlementRegistry` id.
    pub market_registry_id: String,
    pub market_symbol: String,
    /// Resting order digest (orderbook primary key).
    pub digest: String,
    pub price_ticks: u64,
    /// Quote raw units per base raw unit.
    pub price_per_unit: f64,
    /// Base (underlying raw) units resting.
    pub size_units: u64,
    pub order_expiry_ms: u64,
    pub at_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrittenDto {
    pub bucket_id: String,
    pub position_id: String,
    pub asset_coin_type: String,
    pub symbol: Option<String>,
    pub is_put: bool,
    pub strike: String,
    pub strike_scale: u8,
    pub strike_scaled: f64,
    pub expiry_ms: u64,
    pub amount: u64,
    /// Units covered by a held long in the same series.
    pub covered: u64,
    pub naked: u64,
    pub mark: Option<MarkDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReservationsDto {
    pub count: usize,
    pub total: u64,
    /// Live (`quoted` / `accepted`) reservations, soonest-expiry first.
    /// Durable in the history DB and reconstructed at boot (SO-444).
    pub entries: Vec<Reservation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PnlDto {
    pub spread: f64,
    pub scalp: f64,
    pub theta: f64,
    pub funding: f64,
    pub total: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueDto {
    pub name: String,
    pub symbol: String,
    /// True for every venue today: only the `paper` venue exists, so
    /// funding is a config constant and margin headroom a placeholder.
    /// Flips per-venue when a real venue (Bluefin) lands.
    pub simulated: bool,
    /// Signed perp position, underlying units (positive = long — SO-428).
    pub position_units: f64,
    pub funding_rate_annual: f64,
    pub margin_headroom: f64,
    /// |position| × spot, settlement raw.
    pub notional: f64,
    pub realized_pnl: f64,
    /// Cumulative funding paid on the venue (negative = received) — SO-438.
    pub funding_paid: f64,
    /// `None` when a venue read failed this snapshot.
    pub read_ok: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolHedgeDto {
    pub symbol: String,
    pub book_delta_units: f64,
    /// Signed hedge position (positive = long — SO-428).
    pub hedge_units: f64,
    pub net_units: f64,
    /// Current band width, underlying units (funding-widened when
    /// applicable). `None` when spot is unavailable.
    pub band_units: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HedgeDto {
    pub band_pct_nav: f64,
    pub band_wide_pct_nav: f64,
    pub funding_widen_threshold: f64,
    pub interval_secs: u64,
    pub venues: Vec<VenueDto>,
    pub by_symbol: Vec<SymbolHedgeDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketDto {
    pub symbol: String,
    pub coin_type: String,
    pub decimals: u8,
    pub spot: Option<f64>,
    pub spot_at_ms: Option<u64>,
    /// Annualized realized vol, short/long windows (None while cold).
    pub realized_vol_short: Option<f64>,
    pub realized_vol_long: Option<f64>,
    pub fallback_vol: f64,
    pub surface_is_fallback: bool,
    pub carry_yield: f64,
    /// Which estimator quotes the surface (`"windows"` | `"har"`) and the
    /// vol-forecast (live or shadow) it shows (SO-440). Forecast fields
    /// are `None` until the history buffer has a sample.
    pub estimator: &'static str,
    pub regime: Option<String>,
    pub sample_interval_ms: Option<u64>,
    pub sigma_mean: Option<f64>,
    pub sigma_q_bid: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEchoDto {
    pub refresh_secs: u64,
    pub expected_holding_years: f64,
    pub surface: SurfaceTomlConfig,
    pub v1: V1Config,
    pub capital: CapitalConfig,
    pub monitors: MonitorsConfig,
    pub auctions_enabled: bool,
    pub exits_enabled: bool,
    pub listings_enabled: bool,
}

// ── snapshot builder ───────────────────────────────────────────────────

/// Build the snapshot. Reads shared state + the book under short lock
/// scopes, then does the (in-memory) venue reads.
pub async fn snapshot(desk: &Desk, network: &str) -> DeskStateDto {
    let now = super::auctions::now_ms();
    let exposure = desk.shared.exposure.read().clone();
    let marks = desk.shared.marks.read().clone();
    let spots = desk.shared.spots.read().clone();
    let stress = *desk.shared.stress.read();
    let stress_blocked = desk
        .shared
        .stress_blocked
        .load(std::sync::atomic::Ordering::Relaxed);
    let risk_off = desk.shared.risk_off.load(std::sync::atomic::Ordering::Relaxed);
    let book_delta_units = desk.shared.book_delta_units.read().clone();
    let funding_rate_annual = *desk.shared.funding_rate_annual.read();

    let symbol_of = |coin_type: &str| -> Option<String> {
        desk.market_meta
            .iter()
            .find(|m| m.coin_type == coin_type)
            .map(|m| m.symbol.clone())
    };

    // Book reads + net greeks off the refresher's per-unit marks.
    let per_unit: HashMap<protocol_types::ids::ObjectId, Greeks> =
        marks.iter().map(|(id, m)| (*id, m.greeks)).collect();
    let (holdings, written, reservations, pnl, naked_written_units, by_expiry, total_greeks, listed_by_bucket) = {
        let b = desk.book.read();
        let (by_expiry, total) = b.net_greeks(&per_unit);
        let listed: HashMap<protocol_types::ids::ObjectId, u64> = b
            .holdings
            .iter()
            .map(|h| (h.bucket_id, b.listed_units(&h.bucket_id)))
            .collect();
        (
            b.holdings.clone(),
            b.written.clone(),
            b.reservations_snapshot(),
            b.pnl,
            b.naked_written_units(),
            by_expiry,
            total,
            listed,
        )
    };

    let mark_dto = |bucket: &protocol_types::ids::ObjectId, amount: u64| -> Option<MarkDto> {
        marks.get(bucket).map(|m| MarkDto {
            mark_per_unit: m.mark_per_unit,
            value: m.mark_per_unit * amount as f64,
            sigma: m.sigma,
            spot: m.spot,
            at_ms: m.at_ms,
            greeks_per_unit: m.greeks.into(),
        })
    };

    let holdings_dto: Vec<HoldingDto> = holdings
        .iter()
        .map(|h| HoldingDto {
            bucket_id: format!("0x{}", h.bucket_id.to_hex()),
            option_coin_type: h.option_coin_type.clone(),
            asset_coin_type: h.asset_coin_type.clone(),
            symbol: symbol_of(&h.asset_coin_type),
            is_put: h.is_put,
            strike: h.strike.to_string(),
            strike_scale: h.strike_scale,
            strike_scaled: h.strike_scaled(),
            expiry_ms: h.expiry_ms,
            amount_vault: h.amount_vault,
            amount_wallet: h.amount_wallet,
            amount_coin_positions: h.amount_coin_positions(),
            amount: h.amount(),
            listed_units: listed_by_bucket.get(&h.bucket_id).copied().unwrap_or(0),
            mark: mark_dto(&h.bucket_id, h.amount()),
        })
        .collect();
    let mut listings_dto: Vec<ListingDto> = desk
        .shared
        .listings
        .read()
        .iter()
        .map(|(bucket, l)| ListingDto {
            bucket_id: format!("0x{}", bucket.to_hex()),
            market_registry_id: l.market_registry_id.clone(),
            market_symbol: l.market_symbol.clone(),
            digest: l.digest.clone(),
            price_ticks: l.price_ticks,
            price_per_unit: l.price_per_unit,
            size_units: l.size_units,
            order_expiry_ms: l.order_expiry_ms,
            at_ms: l.at_ms,
        })
        .collect();
    listings_dto.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));
    let written_dto: Vec<WrittenDto> = written
        .iter()
        .map(|w| WrittenDto {
            bucket_id: format!("0x{}", w.bucket_id.to_hex()),
            position_id: format!("0x{}", w.position_id.to_hex()),
            asset_coin_type: w.asset_coin_type.clone(),
            symbol: symbol_of(&w.asset_coin_type),
            is_put: w.is_put,
            strike: w.strike.to_string(),
            strike_scale: w.strike_scale,
            strike_scaled: w.strike_scaled(),
            expiry_ms: w.expiry_ms,
            amount: w.amount,
            covered: w.covered,
            naked: w.naked(),
            mark: mark_dto(&w.bucket_id, w.amount),
        })
        .collect();

    let mut by_expiry: Vec<ExpiryGreeksDto> = by_expiry
        .into_iter()
        .map(|(expiry_ms, g)| ExpiryGreeksDto {
            expiry_ms,
            greeks: GreeksAggDto {
                delta_units: g.delta_units,
                gamma_units: g.gamma_units,
                vega: g.vega,
                theta_per_day: g.theta_per_day,
            },
        })
        .collect();
    by_expiry.sort_by_key(|e| e.expiry_ms);

    // Venue roster: read positions/funding/margin through the same path
    // the monitors use; a failed read reports zeros with `read_ok:false`.
    let mut venues = Vec::with_capacity(desk.venue_roster.len());
    for mv in &desk.venue_roster {
        let spot = spots.get(&mv.symbol).map(|s| s.spot).unwrap_or(0.0);
        let realized_pnl = mv.venue.realized_pnl().await.unwrap_or(0.0);
        let funding_paid = mv.venue.funding_paid().await.unwrap_or(0.0);
        match read_venue(mv, spot).await {
            Some(r) => venues.push(VenueDto {
                name: r.name,
                symbol: r.symbol,
                simulated: true,
                position_units: r.position_units,
                funding_rate_annual: r.funding_annual,
                margin_headroom: r.margin_headroom,
                notional: r.notional,
                realized_pnl,
                funding_paid,
                read_ok: true,
            }),
            None => venues.push(VenueDto {
                name: mv.venue.name().to_string(),
                symbol: mv.symbol.clone(),
                simulated: true,
                position_units: 0.0,
                funding_rate_annual: 0.0,
                margin_headroom: 0.0,
                notional: 0.0,
                realized_pnl,
                funding_paid,
                read_ok: false,
            }),
        }
    }
    let by_symbol: Vec<SymbolHedgeDto> = desk
        .market_meta
        .iter()
        .map(|m| {
            let book_delta =
                book_delta_units.get(&m.coin_type).copied().unwrap_or(0.0);
            let hedge_units: f64 = venues
                .iter()
                .filter(|v| v.symbol == m.symbol)
                .map(|v| v.position_units)
                .sum();
            let band_units = spots.get(&m.symbol).map(|s| {
                hedge::band_units(&desk.cfg.hedge, exposure.nav, s.spot, funding_rate_annual)
            });
            SymbolHedgeDto {
                symbol: m.symbol.clone(),
                book_delta_units: book_delta,
                hedge_units,
                net_units: book_delta + hedge_units,
                band_units,
            }
        })
        .collect();

    let markets: Vec<MarketDto> = desk
        .market_meta
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (short, long) = desk.models[i].window_vols();
            let est = desk.models[i].estimator_state();
            let spot = spots.get(&m.symbol);
            MarketDto {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                decimals: m.decimals,
                spot: spot.map(|s| s.spot),
                spot_at_ms: spot.map(|s| s.at_ms),
                realized_vol_short: short,
                realized_vol_long: long,
                fallback_vol: m.fallback_vol,
                surface_is_fallback: desk.models[i].surface_is_fallback(),
                carry_yield: desk.models[i].carry_yield,
                estimator: est.estimator,
                regime: est.regime,
                sample_interval_ms: est.sample_interval_ms,
                sigma_mean: est.sigma_mean,
                sigma_q_bid: est.sigma_q_bid,
            }
        })
        .collect();

    let limits = desk.cfg.limits;
    let capacities = capacities(&limits, &desk.cfg.capital, &exposure.capital, now);
    DeskStateDto {
        generated_at_ms: now,
        booted_at_ms: desk.booted_at_ms,
        network: network.to_string(),
        vault: VaultDto {
            vault_id: desk.vault_id.to_hex_literal(),
            provisioned: desk.provisioned,
            curator_cap: desk
                .curator_refs
                .map(|c| c.curator_cap.to_hex_literal()),
            curator_session_flows_enabled: desk.curator_refs.is_some(),
            mm_release_enabled: desk.cfg.mm_release_enabled,
            settlement_coin_type: desk.settlement_coin_type.clone(),
            settlement_decimals: desk.settlement_decimals,
        },
        utilization: utilization(&limits, &exposure),
        exposure: ExposureDto {
            nav: exposure.nav,
            premium_deployed: exposure.premium_deployed,
            reserved: exposure.reserved,
            net_vega_per_volpt: exposure.net_vega_per_volpt,
            theta_cost_per_day: exposure.theta_cost_per_day,
            premium_by_expiry: exposure.premium_by_expiry.clone(),
            premium_by_strike_bucket: exposure.premium_by_strike_bucket,
            call_premium: exposure.call_premium,
            put_premium: exposure.put_premium,
            delta_units_positive: exposure.delta_units_positive,
            delta_units_negative: exposure.delta_units_negative,
            gamma_units_calls: exposure.gamma_units_calls,
            gamma_units_puts: exposure.gamma_units_puts,
            kill_switch: exposure.kill_switch,
            stress_blocked,
            risk_off,
        },
        limits,
        capital: exposure.capital.clone(),
        capacities,
        greeks: GreeksDto {
            total: GreeksAggDto {
                delta_units: total_greeks.delta_units,
                gamma_units: total_greeks.gamma_units,
                vega: total_greeks.vega,
                theta_per_day: total_greeks.theta_per_day,
            },
            by_expiry,
        },
        book_delta_units,
        naked_written_units,
        funding_rate_annual,
        stress: stress.map(StressDto::from),
        holdings: holdings_dto,
        listings: listings_dto,
        written: written_dto,
        reservations: ReservationsDto {
            count: reservations.len(),
            total: reservations.iter().map(|r| r.amount).sum(),
            entries: reservations,
        },
        pnl: PnlDto {
            spread: pnl.spread,
            scalp: pnl.scalp,
            theta: pnl.theta,
            funding: pnl.funding,
            total: pnl.spread + pnl.scalp + pnl.theta + pnl.funding,
        },
        hedge: HedgeDto {
            band_pct_nav: desk.cfg.hedge.band_pct_nav,
            band_wide_pct_nav: desk.cfg.hedge.band_wide_pct_nav,
            funding_widen_threshold: desk.cfg.hedge.funding_widen_threshold,
            interval_secs: desk.cfg.hedge.interval_secs,
            venues,
            by_symbol,
        },
        markets,
        config: ConfigEchoDto {
            refresh_secs: desk.cfg.refresh_secs,
            expected_holding_years: desk.cfg.expected_holding_years,
            surface: desk.cfg.surface,
            v1: desk.cfg.v1,
            capital: desk.cfg.capital.clone(),
            monitors: desk.cfg.monitors,
            auctions_enabled: desk.cfg.auctions.enabled,
            exits_enabled: desk.cfg.exits.enabled,
            listings_enabled: desk.cfg.listings.enabled,
        },
    }
}

impl From<StressSnapshot> for StressDto {
    fn from(s: StressSnapshot) -> Self {
        Self {
            at_ms: s.at_ms,
            gap_down_60: s.gap_down_60,
            gap_up_80: s.gap_up_80,
            flat_6mo: s.flat_6mo,
            funding_minus_50: s.funding_minus_50,
            worst_drawdown: s.worst_drawdown,
            blocked: s.blocked,
        }
    }
}

/// Headline effective capacities over the snapshot at the reference
/// ratios (doc 08 §4.6); every expiry the book or reservations hold.
pub fn capacities(
    limits: &LimitsConfig,
    cfg: &CapitalConfig,
    snap: &CapitalSnapshot,
    now_ms: u64,
) -> CapacitiesDto {
    let ratios = FillRatios {
        hedge_notional_per_premium: cfg.reference_hedge_notional_per_premium,
        exercise_cash_per_premium: cfg.reference_exercise_cash_per_premium,
    };
    let reference_ratios = ReferenceRatiosDto {
        hedge_notional_per_premium: ratios.hedge_notional_per_premium,
        exercise_cash_per_premium: ratios.exercise_cash_per_premium,
    };
    match snap.risk_nav_at(now_ms) {
        Ok(risk_nav) => {
            let p = CapitalPolicy { limits, snap, risk_nav };
            let mut expiries: Vec<u64> = snap.premium_by_expiry.keys().copied().collect();
            expiries.sort_unstable();
            CapacitiesDto {
                risk_nav: Some(risk_nav),
                stale: None,
                reference_ratios,
                effective_call_capacity: Some(p.call_capacity(&ratios)),
                effective_put_capacity: Some(p.put_capacity(&ratios)),
                effective_expiry_capacity: expiries
                    .into_iter()
                    .map(|expiry_ms| ExpiryCapacityDto {
                        expiry_ms,
                        capacity: p.expiry_capacity(expiry_ms, &ratios),
                    })
                    .collect(),
            }
        }
        Err(stale) => CapacitiesDto {
            risk_nav: None,
            stale: Some(stale.as_str()),
            reference_ratios,
            effective_call_capacity: None,
            effective_put_capacity: None,
            effective_expiry_capacity: Vec::new(),
        },
    }
}

/// Utilizations against the SOFT budgets, mirroring `limits::evaluate`'s
/// ratios with no proposed fill. All 0 when NAV is 0.
pub fn utilization(cfg: &LimitsConfig, x: &super::BookExposure) -> UtilizationDto {
    if x.nav <= 0.0 {
        return UtilizationDto { premium: 0.0, vega: 0.0, theta: 0.0 };
    }
    UtilizationDto {
        premium: (x.premium_deployed + x.reserved)
            / (cfg.premium_budget_soft * x.nav).max(f64::MIN_POSITIVE),
        vega: x.net_vega_per_volpt.abs()
            / (cfg.vega_cap_nav_per_volpt * x.nav).max(f64::MIN_POSITIVE),
        theta: (x.theta_cost_per_day / (cfg.theta_soft_nav_per_day * x.nav).max(f64::MIN_POSITIVE))
            .max(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::BookExposure;

    #[test]
    fn utilization_ratios_match_limits_evaluate_conventions() {
        let cfg = LimitsConfig::default();
        let x = BookExposure {
            nav: 1e9,
            premium_deployed: 1e6,
            reserved: 2e6,
            net_vega_per_volpt: -1e5,
            theta_cost_per_day: 1e4,
            ..Default::default()
        };
        let u = utilization(&cfg, &x);
        assert!((u.premium - 3e6 / 2.5e8).abs() < 1e-12);
        assert!((u.vega - 1e5 / 5e6).abs() < 1e-12);
        assert!((u.theta - 1e4 / 1e6).abs() < 1e-12);
        // NAV 0 → all zeros, no NaNs.
        let z = utilization(&cfg, &BookExposure::default());
        assert_eq!((z.premium, z.vega, z.theta), (0.0, 0.0, 0.0));
    }

    #[test]
    fn state_dto_serializes_camel_case_with_string_strikes() {
        let dto = HoldingDto {
            bucket_id: "0xab".into(),
            option_coin_type: "0x1::c::C".into(),
            asset_coin_type: "0x1::a::A".into(),
            symbol: Some("TBTC".into()),
            is_put: false,
            strike: u128::MAX.to_string(),
            strike_scale: 6,
            strike_scaled: 1.5,
            expiry_ms: 1_700_000_000_000,
            amount_vault: 1,
            amount_wallet: 2,
            amount_coin_positions: 3,
            amount: 6,
            listed_units: 0,
            mark: Some(MarkDto {
                mark_per_unit: 0.5,
                value: 3.0,
                sigma: 0.6,
                spot: 100.0,
                at_ms: 1,
                greeks_per_unit: GreeksPerUnitDto {
                    delta: 0.5,
                    gamma: 0.01,
                    vega: 20.0,
                    theta: -5.0,
                },
            }),
        };
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["strike"], u128::MAX.to_string());
        assert_eq!(v["amountVault"], 1);
        assert_eq!(v["mark"]["greeksPerUnit"]["theta"], -5.0);
        assert_eq!(v["isPut"], false);
    }
}
