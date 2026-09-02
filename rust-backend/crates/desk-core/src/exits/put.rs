//! Put exercise policy (SO-443, doc 08 §4.4 / 00-plan §5): the timing
//! rule, the minimum-profit bound, the three-path route choice and the
//! expiry-safe ladder. Pure; execution is `services/mm-bot`'s
//! `desk::exits::put`.
//!
//! A held put is exercised only when
//!
//! ```text
//! strike payout − underlying acquisition cost − swap fees/slippage
//!   − flash cost − gas ≥ min_profit
//! min_profit = max($10 settlement, 5 bps × payout, 2 × route uncertainty)
//! ```
//!
//! and then through the FIRST profitable AVAILABLE atomic route, in
//! order: own/vault underlying → base flash loan → quote flash loan
//! ([`PutPath::ORDER`]). Large positions ladder in slices sized so the
//! ladder cannot cross expiry.

use serde::Deserialize;

use crate::model::MarketModel;

use super::ExitsConfig;

// ── route vocabulary (shared with `sui_tx::tx::put_exercise`) ─────────

/// DeepBook v3 `FLOAT_SCALING`: raw price = quote-atomic per base-atomic
/// × 10^9.
pub const FLOAT_SCALING: u128 = 1_000_000_000;

/// The three atomic put-exercise routes, in policy order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutPath {
    VaultUnderlying,
    BaseFlash,
    QuoteFlash,
}

impl PutPath {
    pub const ORDER: [PutPath; 3] =
        [PutPath::VaultUnderlying, PutPath::BaseFlash, PutPath::QuoteFlash];

    pub fn label(self) -> &'static str {
        match self {
            PutPath::VaultUnderlying => "vault_underlying",
            PutPath::BaseFlash => "base_flash",
            PutPath::QuoteFlash => "quote_flash",
        }
    }
}

/// What the spot pool can do for a put exercise right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolLiquidity {
    /// `pool::vault_balances` base — the base flash-loan capacity.
    pub base_balance: u64,
    /// `pool::vault_balances` quote — the quote flash-loan capacity.
    pub quote_balance: u64,
    pub lot_size: u64,
    pub min_size: u64,
    /// Asks best-first: `(price_raw, base_quantity)`.
    pub asks: Vec<(u64, u64)>,
}

/// Round `amount` up to the pool's lot — the base a buy must take so the
/// swap returns at least `amount`. `None` when it rounds below `min_size`.
pub fn lot_round_up(amount: u64, lot_size: u64, min_size: u64) -> Option<u64> {
    let lot = lot_size.max(1);
    let rounded = amount.div_ceil(lot).checked_mul(lot)?;
    (rounded >= min_size).then_some(rounded)
}

/// Settlement needed to lift `base_needed` off the ask ladder (ceil per
/// level), `None` when the ladder is too shallow.
pub fn quote_needed_for_base(asks: &[(u64, u64)], base_needed: u64) -> Option<u64> {
    let mut remaining = base_needed;
    let mut cost: u128 = 0;
    for &(price_raw, qty) in asks {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(qty);
        cost += (take as u128 * price_raw as u128).div_ceil(FLOAT_SCALING);
        remaining -= take;
    }
    (remaining == 0).then(|| u64::try_from(cost).unwrap_or(u64::MAX))
}

// ── config ─────────────────────────────────────────────────────────────

/// `[desk.exits.put]` knobs. Defaults per doc 08 §0.4.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PutExerciseConfig {
    pub enabled: bool,
    /// Minimum-profit term 1: settlement-equivalent dollars.
    pub min_profit_usd: f64,
    /// Minimum-profit term 2: bps of the strike payout.
    pub min_profit_bps: f64,
    /// Route uncertainty as a bps bound on the strike payout (Pyth
    /// confidence/staleness, router quote age, tick rounding,
    /// dev-inspect-to-inclusion drift). Also the repurchase slippage
    /// allowance folded into the PTB's explicit max-input.
    pub route_uncertainty_bps: f64,
    /// Minimum-profit term 3: this × route uncertainty.
    pub route_uncertainty_mult: f64,
    /// Taker fee bound on the repurchase, bps of acquisition cost
    /// (whitelisted DeepBook pools charge none).
    pub swap_fee_bps: f64,
    /// Flash-loan fee bound, bps of principal (DeepBook v3: none).
    pub flash_fee_bps: f64,
    /// Gas allowance per slice, settlement-equivalent dollars.
    pub gas_cost_usd: f64,
    /// Dev-inspect gas (MIST) above which a slice is not submitted.
    pub max_gas_mist: u64,
    /// Per-slice inclusion allowance when sizing a ladder against expiry.
    pub ladder_tx_secs: u64,
    /// No slice starts inside this margin before expiry.
    pub expiry_margin_secs: u64,
    /// Ask-ladder depth read for route pricing.
    pub ask_ticks: u64,
}

impl Default for PutExerciseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_profit_usd: 10.0,
            min_profit_bps: 5.0,
            route_uncertainty_bps: 20.0,
            route_uncertainty_mult: 2.0,
            swap_fee_bps: 0.0,
            flash_fee_bps: 0.0,
            gas_cost_usd: 0.05,
            max_gas_mist: 100_000_000,
            ladder_tx_secs: 30,
            expiry_margin_secs: 120,
            ask_ticks: 50,
        }
    }
}

// ── pure policy ────────────────────────────────────────────────────────

/// `put_bucket::exercise_payout` mirror: floor(amount × strike).
pub fn strike_payout(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    u64::try_from(amount as u128 * strike / divisor).unwrap_or(u64::MAX)
}

fn bps_of(amount: u64, bps: f64) -> u64 {
    (amount as f64 * bps / 10_000.0).ceil().max(0.0) as u64
}

fn usd_raw(usd: f64, settlement_decimals: u8) -> u64 {
    (usd * 10f64.powi(settlement_decimals as i32)).ceil().max(0.0) as u64
}

/// `max($ term, bps × payout, mult × route uncertainty)` in settlement
/// raw units.
pub fn min_profit(cfg: &PutExerciseConfig, payout: u64, settlement_decimals: u8) -> u64 {
    usd_raw(cfg.min_profit_usd, settlement_decimals)
        .max(bps_of(payout, cfg.min_profit_bps))
        .max(bps_of(route_uncertainty(cfg, payout), cfg.route_uncertainty_mult * 10_000.0))
}

/// The conservative cash bound on route drift for one slice.
pub fn route_uncertainty(cfg: &PutExerciseConfig, payout: u64) -> u64 {
    bps_of(payout, cfg.route_uncertainty_bps)
}

/// What the desk can draw on for one slice.
#[derive(Clone, Debug, Default)]
pub struct PutLiquidity {
    /// Underlying already held by the leg (wallet float, or vault free
    /// balance for the custody leg) — the vault-underlying route.
    pub own_underlying: u64,
    /// The allowlisted UNDERLYING/SETTLEMENT spot pool.
    pub pool: PoolLiquidity,
}

/// One slice's chosen route and bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutPlan {
    pub path: PutPath,
    pub amount: u64,
    pub payout: u64,
    /// Explicit max settlement spent on the repurchase (acquisition +
    /// fee + slippage allowance); the quote-flash principal.
    pub max_quote_in: u64,
    pub min_profit: u64,
    /// Modeled net after every cost, before the minimum.
    pub expected_net: u64,
}

/// Why no route was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanReject {
    /// The ask ladder can't supply the replacement underlying.
    NoRoute,
    /// Modeled net is below the minimum on every route.
    Profit { net: i128, min_profit: u64 },
    /// Every profitable route lacks capacity (own underlying / flash).
    Capacity,
}

impl std::fmt::Display for PlanReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanReject::NoRoute => write!(f, "no repurchase route (ask ladder too shallow)"),
            PlanReject::Profit { net, min_profit } => {
                write!(f, "modeled net {net} below minimum profit {min_profit}")
            }
            PlanReject::Capacity => write!(f, "no route has capacity"),
        }
    }
}

/// Choose the first profitable available route for `amount` put units.
pub fn plan_slice(
    cfg: &PutExerciseConfig,
    amount: u64,
    strike: u128,
    strike_scale: u8,
    settlement_decimals: u8,
    liq: &PutLiquidity,
) -> Result<PutPlan, PlanReject> {
    let payout = strike_payout(amount, strike, strike_scale);
    let base_needed = lot_round_up(amount, liq.pool.lot_size, liq.pool.min_size)
        .ok_or(PlanReject::NoRoute)?;
    let acquisition = quote_needed_for_base(&liq.pool.asks, base_needed)
        .ok_or(PlanReject::NoRoute)?;
    let swap_fee = bps_of(acquisition, cfg.swap_fee_bps);
    let uncertainty = route_uncertainty(cfg, payout);
    let max_quote_in = acquisition.saturating_add(swap_fee).saturating_add(uncertainty);
    let min_profit = min_profit(cfg, payout, settlement_decimals);
    let gas = usd_raw(cfg.gas_cost_usd, settlement_decimals);

    let mut best_reject = PlanReject::Capacity;
    for path in PutPath::ORDER {
        let flash_cost = match path {
            PutPath::VaultUnderlying => 0,
            PutPath::BaseFlash => bps_of(acquisition, cfg.flash_fee_bps),
            PutPath::QuoteFlash => bps_of(max_quote_in, cfg.flash_fee_bps),
        };
        let net = payout as i128
            - acquisition as i128
            - swap_fee as i128
            - flash_cost as i128
            - gas as i128;
        // The on-chain assertion needs the whole max-input plus the
        // minimum to fit inside the payout.
        let onchain_ok = max_quote_in.saturating_add(min_profit) <= payout;
        if net < min_profit as i128 || !onchain_ok {
            best_reject = PlanReject::Profit { net, min_profit };
            continue;
        }
        let available = match path {
            PutPath::VaultUnderlying => liq.own_underlying >= amount,
            PutPath::BaseFlash => liq.pool.base_balance >= amount,
            PutPath::QuoteFlash => liq.pool.quote_balance >= max_quote_in,
        };
        if !available {
            continue;
        }
        return Ok(PutPlan {
            path,
            amount,
            payout,
            max_quote_in,
            min_profit,
            expected_net: (net - min_profit as i128).max(0) as u64 + min_profit,
        });
    }
    Err(best_reject)
}

/// Slice `amount` into a ladder that cannot cross expiry: at most
/// `max_slice` per tx, and no more slices than fit in
/// `remaining_ms − margin` at `tx_ms` each (bigger slices when the
/// clock, not the chunk, binds). Empty when expiry is inside the margin.
pub fn ladder(
    amount: u64,
    max_slice: u64,
    remaining_ms: u64,
    tx_ms: u64,
    margin_ms: u64,
) -> Vec<u64> {
    if amount == 0 || remaining_ms <= margin_ms {
        return Vec::new();
    }
    let max_slice = max_slice.max(1);
    let allowed = ((remaining_ms - margin_ms) / tx_ms.max(1)).max(1);
    let wanted = amount.div_ceil(max_slice);
    let n = wanted.min(allowed);
    let slice = amount.div_ceil(n);
    let mut out = Vec::with_capacity(n as usize);
    let mut left = amount;
    while left > 0 {
        let s = left.min(slice);
        out.push(s);
        left -= s;
    }
    out
}

/// The exercise-timing rule for a held put: ITM, and either inside the
/// near-expiry sweep window (every economically exercisable put is
/// attempted before expiry) or American-optimal — carry on the strike
/// cash beats the remaining time value with margin. Pure; the caller
/// prices `carry` and `time_value` per unit.
pub fn put_exercise_wanted(
    cfg: &ExitsConfig,
    spot: f64,
    strike: f64,
    hours_to_expiry: f64,
    carry: f64,
    time_value: f64,
) -> bool {
    let itm = spot < strike;
    let near_expiry = hours_to_expiry <= cfg.near_expiry_hours;
    itm && (near_expiry || carry >= time_value * cfg.carry_mult)
}

/// Per-unit carry and time value of a held put under `model`.
pub fn put_carry_and_time_value(
    model: &MarketModel,
    spot: f64,
    strike: f64,
    t_years: f64,
) -> (f64, f64) {
    let (sigma, _) = model.sigma(spot, strike, t_years);
    let fair = model.fair_per_unit(true, spot, strike, t_years, sigma);
    let intrinsic = (strike - spot).max(0.0);
    let carry = strike * model.rate * t_years;
    (carry, (fair - intrinsic).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PutExerciseConfig {
        PutExerciseConfig::default()
    }

    /// 9-dec underlying, 6-dec settlement, strike $4.00/unit = 0.004
    /// settlement-raw per underlying-raw → 4_000 at scale 6 (the
    /// `strike_cost` convention): 5 units pay out $20.
    const STRIKE: u128 = 4_000;
    const SCALE: u8 = 6;
    const DEC: u8 = 6;
    const AMOUNT: u64 = 5_000_000_000;

    /// Ask ladder at $3.50/unit: 0.0035 quote-atomic per base-atomic →
    /// price_raw 3_500_000 (× 1e9).
    fn pool(base_balance: u64, quote_balance: u64) -> PoolLiquidity {
        PoolLiquidity {
            base_balance,
            quote_balance,
            lot_size: 1_000,
            min_size: 10_000,
            asks: vec![(3_500_000, 1_000_000_000_000)],
        }
    }

    #[test]
    fn payout_mirrors_apply_strike_floor() {
        assert_eq!(strike_payout(AMOUNT, STRIKE, SCALE), 20_000_000);
        assert_eq!(strike_payout(1, 15, 1), 1); // 1.5 floors to 1
    }

    #[test]
    fn min_profit_is_the_max_of_the_three_terms() {
        let c = cfg();
        // $10 dominates a $20 payout (5 bps = $0.01, 2 × 20 bps = $0.08).
        assert_eq!(min_profit(&c, 20_000_000, DEC), 10_000_000);
        // A $1m payout: 5 bps = $500, 2 × 20 bps = $4000 → uncertainty term.
        assert_eq!(min_profit(&c, 1_000_000_000_000, DEC), 4_000_000_000);
        // Uncertainty off: bps term wins.
        let c2 = PutExerciseConfig { route_uncertainty_bps: 0.0, ..cfg() };
        assert_eq!(min_profit(&c2, 1_000_000_000_000, DEC), 500_000_000);
    }

    #[test]
    fn selects_vault_underlying_first_when_held() {
        let liq = PutLiquidity { own_underlying: AMOUNT, pool: pool(AMOUNT, 1_000_000_000) };
        // Payout $20 (5 × $4); acquisition 5 × $3.50 = $17.50; gas $0.05;
        // net $2.45 < $10 minimum → not profitable. Use 50 units instead.
        let big = AMOUNT * 50;
        let liq = PutLiquidity { own_underlying: big, ..liq };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::VaultUnderlying);
        assert_eq!(plan.payout, 1_000_000_000); // $1000
        // acquisition $875 + 20 bps of $1000 ($2) slippage allowance.
        assert_eq!(plan.max_quote_in, 875_000_000 + 2_000_000);
        assert_eq!(plan.min_profit, 10_000_000);
        assert!(plan.max_quote_in + plan.min_profit <= plan.payout);
    }

    #[test]
    fn falls_through_to_base_flash_then_quote_flash() {
        let big = AMOUNT * 50;
        // No own underlying, base capacity present → base flash.
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big, 0) };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::BaseFlash);
        // Base liquidity unavailable → quote flash (needs max_quote_in of
        // quote capacity).
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big - 1, 877_000_000) };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::QuoteFlash);
        // Neither flash side has capacity → Capacity reject.
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big - 1, 876_999_999) };
        assert_eq!(plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq), Err(PlanReject::Capacity));
    }

    #[test]
    fn profit_bound_rejects_every_route() {
        // 5 units: net $2.45 < $10.
        let liq = PutLiquidity { own_underlying: AMOUNT, pool: pool(AMOUNT, 1_000_000_000) };
        match plan_slice(&cfg(), AMOUNT, STRIKE, SCALE, DEC, &liq) {
            Err(PlanReject::Profit { net, min_profit }) => {
                assert_eq!(net, 20_000_000 - 17_500_000 - 50_000);
                assert_eq!(min_profit, 10_000_000);
            }
            other => panic!("{other:?}"),
        }
        // OTM-ish: asks above strike → negative net.
        let mut deep = pool(AMOUNT * 50, 0);
        deep.asks = vec![(4_500_000, u64::MAX / 4)];
        let liq = PutLiquidity { own_underlying: AMOUNT * 50, pool: deep };
        assert!(matches!(
            plan_slice(&cfg(), AMOUNT * 50, STRIKE, SCALE, DEC, &liq),
            Err(PlanReject::Profit { net, .. }) if net < 0
        ));
    }

    #[test]
    fn shallow_ladder_or_sub_min_size_is_no_route() {
        let mut shallow = pool(AMOUNT * 50, 0);
        shallow.asks = vec![(3_500_000, 1_000)];
        let liq = PutLiquidity { own_underlying: AMOUNT * 50, pool: shallow };
        let r = plan_slice(&cfg(), AMOUNT * 50, STRIKE, SCALE, DEC, &liq);
        assert_eq!(r, Err(PlanReject::NoRoute));
        let liq = PutLiquidity { own_underlying: 1, pool: pool(1, 1) };
        assert_eq!(plan_slice(&cfg(), 1, STRIKE, SCALE, DEC, &liq), Err(PlanReject::NoRoute));
    }

    #[test]
    fn ladder_reconciles_and_never_crosses_expiry() {
        // 10 chunks of 1e9 fit in 10 min with 30 s per tx.
        let s = ladder(10_000_000_000, 1_000_000_000, 600_000, 30_000, 120_000);
        assert_eq!(s.len(), 10);
        assert_eq!(s.iter().sum::<u64>(), 10_000_000_000);
        // Only 4 tx fit (4 × 30 s inside 240 s − 120 s margin = 4) →
        // bigger slices, same total.
        let s = ladder(10_000_000_000, 1_000_000_000, 240_000, 30_000, 120_000);
        assert_eq!(s.len(), 4);
        assert_eq!(s.iter().sum::<u64>(), 10_000_000_000);
        assert!(s.iter().all(|&x| x <= 2_500_000_000));
        // Uneven remainder still reconciles.
        let s = ladder(7, 3, 1_000_000, 1, 0);
        assert_eq!(s, vec![3, 3, 1]);
        // Inside the expiry margin: nothing starts.
        assert!(ladder(7, 3, 100_000, 30_000, 120_000).is_empty());
        assert!(ladder(0, 3, 1_000_000, 30_000, 0).is_empty());
    }

    #[test]
    fn sweep_attempts_every_itm_put_near_expiry() {
        let c = ExitsConfig::default(); // near_expiry_hours 24, carry_mult 1.1
        // Fixture holdings: (spot, strike, hours, carry, time_value).
        let fixtures = [
            (3.0, 4.0, 1.0, 0.0, 0.5),   // ITM, near expiry, lots of TV → attempted
            (3.9, 4.0, 23.9, 0.0, 0.01), // ITM, just inside the window → attempted
            (3.0, 4.0, 48.0, 0.0, 0.5),  // ITM, far, TV > carry → hold
            (3.0, 4.0, 48.0, 0.6, 0.5),  // ITM, far, carry beats TV → attempted
            (5.0, 4.0, 1.0, 0.0, 0.0),   // OTM near expiry → never
        ];
        let got: Vec<bool> = fixtures
            .iter()
            .map(|&(s, k, h, carry, tv)| put_exercise_wanted(&c, s, k, h, carry, tv))
            .collect();
        assert_eq!(got, [true, true, false, true, false]);
        // Every economically exercisable (ITM) put inside the window is
        // attempted regardless of its remaining time value.
        for tv in [0.0, 0.1, 1.0, 10.0] {
            assert!(put_exercise_wanted(&c, 3.0, 4.0, 0.5, 0.0, tv));
        }
    }

    /// Parity with the backtester's mirror of this policy (doc 08 P3
    /// gate, SO-455): both crates assert the same route goldens.
    #[test]
    fn put_route_goldens_match_the_shared_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../backtester/fixtures/put_route_goldens.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let g: serde_json::Value = serde_json::from_str(&text).unwrap();
        let cfg_of = |v: &serde_json::Value| -> PutExerciseConfig {
            let mut c = PutExerciseConfig::default();
            if let Some(x) = v["swap_fee_bps"].as_f64() {
                c.swap_fee_bps = x;
            }
            if let Some(x) = v["flash_fee_bps"].as_f64() {
                c.flash_fee_bps = x;
            }
            if let Some(x) = v["route_uncertainty_bps"].as_f64() {
                c.route_uncertainty_bps = x;
            }
            c
        };
        for case in g["plan"].as_array().unwrap() {
            let c = cfg_of(&case["cfg"]);
            let liq = PutLiquidity {
                own_underlying: case["own_underlying"].as_u64().unwrap(),
                pool: PoolLiquidity {
                    base_balance: case["base_balance"].as_u64().unwrap(),
                    quote_balance: case["quote_balance"].as_u64().unwrap(),
                    lot_size: case["lot_size"].as_u64().unwrap(),
                    min_size: case["min_size"].as_u64().unwrap(),
                    asks: case["asks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|a| (a[0].as_u64().unwrap(), a[1].as_u64().unwrap()))
                        .collect(),
                },
            };
            let got = plan_slice(
                &c,
                case["amount"].as_u64().unwrap(),
                case["strike"].as_u64().unwrap() as u128,
                case["strike_scale"].as_u64().unwrap() as u8,
                case["settlement_decimals"].as_u64().unwrap() as u8,
                &liq,
            );
            let name = case["name"].as_str().unwrap();
            let exp = &case["expect"];
            match got {
                Ok(plan) => {
                    assert_eq!(plan.path.label(), exp["path"].as_str().unwrap(), "{name}");
                    assert_eq!(plan.payout, exp["payout"].as_u64().unwrap(), "{name}");
                    assert_eq!(plan.max_quote_in, exp["max_quote_in"].as_u64().unwrap(), "{name}");
                    assert_eq!(plan.min_profit, exp["min_profit"].as_u64().unwrap(), "{name}");
                }
                Err(r) => {
                    let label = match &r {
                        PlanReject::NoRoute => "no_route",
                        PlanReject::Profit { .. } => "profit",
                        PlanReject::Capacity => "capacity",
                    };
                    assert_eq!(label, exp["reject"].as_str().unwrap(), "{name}: {r:?}");
                    if let PlanReject::Profit { net, min_profit } = r {
                        if let Some(n) = exp["net"].as_i64() {
                            assert_eq!(net, n as i128, "{name}");
                        }
                        if let Some(m) = exp["min_profit"].as_u64() {
                            assert_eq!(min_profit, m, "{name}");
                        }
                        if exp["net_negative"].as_bool() == Some(true) {
                            assert!(net < 0, "{name}");
                        }
                    }
                }
            }
        }
        for case in g["ladder"].as_array().unwrap() {
            let got = ladder(
                case["amount"].as_u64().unwrap(),
                case["max_slice"].as_u64().unwrap(),
                case["remaining_ms"].as_u64().unwrap(),
                case["tx_ms"].as_u64().unwrap(),
                case["margin_ms"].as_u64().unwrap(),
            );
            let exp: Vec<u64> = case["expect"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            assert_eq!(got, exp, "{}", case["name"]);
        }
        for case in g["min_profit"].as_array().unwrap() {
            let c = cfg_of(&case["cfg"]);
            assert_eq!(min_profit(&c, case["payout"].as_u64().unwrap(), DEC), case["expect"].as_u64().unwrap(), "{}", case["name"]);
        }
        let exits = ExitsConfig::default();
        for case in g["put_wanted"].as_array().unwrap() {
            let f = |k: &str| case[k].as_f64().unwrap();
            assert_eq!(
                put_exercise_wanted(&exits, f("spot"), f("strike"), f("hours"), f("carry"), f("time_value")),
                case["expect"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
}
