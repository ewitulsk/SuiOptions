//! The backtester's seat at the shared strategy kernel (doc 08 §2 /
//! §5.2, SO-450): a [`Scenario`] becomes a [`KernelConfig`] and the
//! engine can construct a [`DeskKernel`] and drive it with its own
//! clock-ordered events. v0's engine still prices with the pure
//! `pricing` functions directly; the switch-over to `on_event` for
//! every decision is doc 08 PR J/O (exact ledger + attribution runner).

use std::collections::HashMap;
use std::sync::Arc;

use desk_core::exits::ExitsConfig;
use desk_core::hedge::HedgeConfig;
use desk_core::limits::{CapitalConfig, LimitsConfig};
use desk_core::model::{MarketModel, SurfaceConfig, V1BidParams};
use desk_core::{DeskKernel, KernelConfig, RollingVolBuffer};
use parking_lot::RwLock;

use crate::scenario::Scenario;

/// Stress gaps the composition surfaces are sized for (the live
/// `[desk.monitors]` defaults: −60% / +80%).
const STRESS_GAP_DOWN: f64 = 0.60;
const STRESS_GAP_UP: f64 = 0.80;

/// The kernel configuration a scenario implies. Limits the scenario does
/// not model keep the desk defaults; composition throttles are off
/// (`composition_penalty_volpts = 0`, as in v0's bid).
pub fn kernel_config(s: &Scenario, settlement_decimals: u8) -> KernelConfig {
    KernelConfig {
        v1: V1BidParams {
            base_spread_volpts: s.bid.base_spread_volpts,
            size_penalty_volpts_per_pct_nav: s.bid.size_penalty_volpts_per_pct_nav,
            size_penalty_quadratic_from_pct: s.bid.size_penalty_quadratic_from_pct,
            inventory_penalty_max_volpts: s.bid.inventory_penalty_max_volpts,
            inventory_penalty_start_util: s.bid.inventory_penalty_start_util,
            max_single_fill_pct_nav: s.bid.max_single_fill_pct_nav,
            funding_income_credit: s.bid.funding_income_credit,
            composition_penalty_volpts: 0.0,
        },
        limits: LimitsConfig {
            premium_budget_hard: s.limits.premium_budget_hard,
            call_premium_max: s.limits.call_premium_max,
            put_premium_max: s.limits.put_premium_max,
            per_expiry_max: s.limits.per_expiry_max,
            vega_cap_nav_per_volpt: s.limits.vega_cap_nav_per_volpt,
            ..LimitsConfig::default()
        },
        capital: CapitalConfig::default(),
        hedge: HedgeConfig {
            band_pct_nav: s.hedge.band_pct_nav,
            band_wide_pct_nav: s.hedge.band_wide_pct_nav,
            funding_widen_threshold: s.hedge.funding_widen_threshold,
            taker_fee_bps: s.hedge.taker_fee_bps,
            fixed_fee_per_fill: s.hedge.fixed_fee_per_fill,
            rebalance_turnover_per_year: s.hedge.rebalance_turnover_per_year,
            margin_financing_rate_annual: s.hedge.margin_financing_rate_annual,
            initial_margin_fraction: s.hedge.initial_margin_fraction,
            ..HedgeConfig::default()
        },
        exits: ExitsConfig::default(),
        quote_ttl_ms: 30_000,
        expected_holding_years: s.bid.expected_holding_years,
        stress_gap_down: STRESS_GAP_DOWN,
        stress_gap_up: STRESS_GAP_UP,
        primary_slippage_bps: s.hedge.slippage_bps,
        settlement_decimals,
        curator_session: false,
        deepbook_adapter: false,
    }
}

/// One simulated market's pricing model on fresh rolling-vol buffers
/// (the scenario's surface shape and fallback vol). The engine feeds the
/// buffers from its decision prices.
pub fn market_model(s: &Scenario, symbol: &str, coin_type: &str) -> MarketModel {
    let e = &s.estimator;
    MarketModel::new(
        symbol.to_string(),
        coin_type.to_string(),
        Arc::new(RwLock::new(RollingVolBuffer::new((e.short_window_hours * 3_600_000.0) as u64))),
        Arc::new(RwLock::new(RollingVolBuffer::new((e.long_window_hours * 3_600_000.0) as u64))),
        e.fallback_vol,
        s.carry_yield,
        0.0,
        SurfaceConfig {
            risk_premium: e.risk_premium,
            skew: e.skew,
            convexity: e.convexity,
            term_short_boost: e.term_short_boost,
            term_decay_years: e.term_decay_years,
            anchor_ratio: None,
            floor_vol: e.floor_vol,
            cap_vol: e.cap_vol,
            short_window_weight: e.short_window_weight,
            long_window_weight: e.long_window_weight,
        },
    )
}

/// A kernel over one market at the scenario's starting NAV, ready for
/// `on_event`.
pub fn kernel(s: &Scenario, symbol: &str, coin_type: &str, booted_at_ms: u64) -> DeskKernel {
    let settlement_decimals = 6;
    DeskKernel::new(
        kernel_config(s, settlement_decimals),
        vec![market_model(s, symbol, coin_type)],
        desk_core::book::Book::new(s.nav0 as u64),
        0.0,
        false,
        booted_at_ms,
    )
}

/// Sim-shaped `MarkUpdate`: the backtester has no chain, so free
/// settlement is the whole NAV, appraisals are always fresh, nothing is
/// queued for withdrawal and there is no external account.
pub fn mark_update(nav: u64, spot: f64, at_ms: u64) -> desk_core::Event {
    desk_core::Event::MarkUpdate(Box::new(desk_core::kernel::MarkUpdate {
        at_ms,
        holdings: None,
        written: None,
        nav: Some(nav),
        appraisal_at: Some(at_ms),
        risk_off: Some(false),
        spot_by_model: vec![Some(spot)],
        free_settlement: nav as f64,
        free_underlying_by_asset: HashMap::new(),
        external: None,
        queued_withdrawal_value: Some(0.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_core::quote::RfqInputs;
    use desk_core::{Command, Event};
    use protocol_types::sides::Side;

    const DAY_MS: u64 = 86_400_000;
    const T0: u64 = 1_788_220_800_000;

    /// Smoke (doc 08 PR I): the backtester constructs the shared kernel
    /// from a scenario and drives it — a writer RFQ quotes, a hedge tick
    /// against the resulting delta trades, and the same trace is
    /// byte-identical on a second run.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn backtester_constructs_and_drives_the_kernel() {
        let mut s = Scenario::default();
        s.nav0 = 1_000_000.0;
        // Tight band so a single 40k-unit fill's delta (~20k) trades.
        s.hedge.band_pct_nav = 0.5;
        let run = || {
            let mut k = kernel(&s, "SUI", "0x2::sui::SUI", T0);
            let mut out = Vec::new();
            out.extend(k.on_event(Event::Spot { market: 0, spot: 3.0, at_ms: T0 }));
            out.extend(k.on_event(mark_update(1_000_000, 3.0, T0)));
            out.extend(k.on_event(Event::Rfq {
                request_id: "sim-1".into(),
                side: Side::Writer,
                market: 0,
                inputs: RfqInputs {
                    write_amount: 40_000,
                    is_put: false,
                    strike: 3,
                    strike_scale: 0,
                    expiry_ms: T0 + 30 * DAY_MS,
                },
                spot: 3.0,
                reserve: Some(1),
                at_ms: T0 + 1,
            }));
            // The fill becomes book delta once the (sim) custody lands;
            // stand it in directly for the smoke.
            k.markets[0].book_delta_units = 20_000.0;
            out.extend(k.on_event(Event::HedgeTick {
                market: 0,
                position_units: 0.0,
                funding_rate_annual: 0.0,
                at_ms: T0 + 2,
            }));
            out
        };
        let a = run();
        assert!(matches!(&a[0], Command::Quote { request_id, .. } if request_id == "sim-1"), "{a:?}");
        assert!(a.iter().any(|c| matches!(c, Command::ReservePremium(r) if r.key == "sim-1")));
        assert!(
            a.iter().any(|c| matches!(c, Command::SubmitHedgeOrder { order, .. } if order.size_units == -20_000.0)),
            "{a:?}"
        );
        assert_eq!(format!("{a:?}"), format!("{:?}", run()), "determinism");
    }
}
