//! Bluefin isolated-margin rules (doc 08 §7.3), verified against the
//! public docs on 2026-09-02:
//!
//! - <https://learn.bluefin.io/bluefin/bluefin-perps-exchange/trading/isolated-margining>
//!   ```text
//!   Position Balance = Size × Oracle Price
//!   Position Debt    = Size × Entry Price − Margin   (long)
//!                    = Size × Entry Price + Margin   (short)
//!   Margin Ratio     = 1 − Debt / Balance            (long)
//!                    = Debt / Balance − 1            (short)
//!   Max Initial Leverage = 1 / IMR ; Entry Margin = Entry Notional / Entry Leverage
//!   ```
//! - <https://learn.bluefin.io/bluefin/bluefin-perps-exchange/trading/risk-engine/liquidation-process>
//!   ```text
//!   liquidatable when MR < MMR
//!   P_liquidation = Debt / (Size × (1 − MMR))  (long)
//!                 = Debt / (Size × (1 + MMR))  (short)
//!   P_bankruptcy  = Debt / Size
//!   the position is closed and the trader loses all margin assigned to it;
//!   liquidation premium > 0: 30% to the insurance fund, 70% to the liquidator
//!   ```
//! - <https://learn.bluefin.io/bluefin/bluefin-perps-exchange/trading/contract-specs>
//!   SUI-PERP: IMR 4.5% (20x), MMR 2.5%, maker 0.01%, taker 0.035%, step
//!   size 1 SUI, min order 1 SUI, default leverage 10x, insurance fund
//!   fee 30%, 2% market take protection from the oracle price.
//!
//! Partial liquidation is NOT specified by the docs; the `partial_close`
//! knob below is a labeled assumption (default off = full liquidation as
//! documented). Binance mark/funding history stands in for Bluefin's
//! (`proxy_venue = true`); these rules size the stress, they do not prove
//! a Bluefin account would have survived (doc 08 §7.3).

use serde::{Deserialize, Serialize};

/// `[margin]` in the scenario. Defaults are SUI-PERP contract specs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct MarginConfig {
    /// `false` = no margin model at all (no entry margin, no top-ups, no
    /// liquidation): the doc 07 / doc 10-v0 assumption, for reproduction
    /// runs only. Every summary labels it.
    pub enabled: bool,
    /// Initial margin ratio (max leverage = 1 / IMR).
    pub imr: f64,
    /// Maintenance margin ratio: liquidation below it.
    pub mmr: f64,
    /// Leverage the desk opens positions at (entry margin = notional /
    /// leverage); capped at `1 / imr`.
    pub leverage: f64,
    /// Top up when MR falls below this (MMR + a buffer).
    pub topup_trigger_mr: f64,
    /// Top up back to this ratio (`0` = the entry ratio `1 / leverage`).
    pub topup_target_mr: f64,
    /// Vault → venue transfer latency for a top-up, ms (assumed).
    pub topup_transfer_ms: i64,
    /// Doc 08 §0.4: maximum top-ups in any 24 h, fraction of NAV.
    pub max_topup_24h_pct_nav: f64,
    /// Margin check cadence, seconds.
    pub check_secs: i64,
    /// ASSUMPTION (not in the Bluefin docs): fraction of the position a
    /// liquidation closes; `0` = full liquidation as documented.
    pub partial_close: f64,
    /// Penalty on the closed notional under a partial liquidation, bps
    /// (assumption; a full liquidation forfeits the whole margin).
    pub partial_penalty_bps: f64,
    /// Venue outage windows `[start_ms, end_ms)`: no orders, no top-ups.
    pub outages: Vec<[i64; 2]>,
}

impl Default for MarginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            imr: 0.045,
            mmr: 0.025,
            leverage: 10.0,
            topup_trigger_mr: 0.035,
            topup_target_mr: 0.0,
            topup_transfer_ms: 30_000,
            max_topup_24h_pct_nav: 0.10,
            check_secs: 60,
            partial_close: 0.0,
            partial_penalty_bps: 0.0,
            outages: Vec::new(),
        }
    }
}

impl MarginConfig {
    pub fn entry_ratio(&self) -> f64 {
        (1.0 / self.leverage.max(1e-9)).max(self.imr)
    }

    pub fn target_ratio(&self) -> f64 {
        if self.topup_target_mr > 0.0 {
            self.topup_target_mr
        } else {
            self.entry_ratio()
        }
    }

    pub fn in_outage(&self, ms: i64) -> bool {
        self.outages.iter().any(|w| ms >= w[0] && ms < w[1])
    }
}

/// One isolated position: signed size, average entry, assigned margin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IsolatedPosition {
    pub size: f64,
    pub entry: f64,
    pub margin: f64,
}

impl IsolatedPosition {
    pub fn balance(&self, mark: f64) -> f64 {
        self.size.abs() * mark
    }

    /// `Size × Entry ∓ Margin` (long subtracts, short adds).
    pub fn debt(&self) -> f64 {
        let notional = self.size.abs() * self.entry;
        if self.size > 0.0 {
            notional - self.margin
        } else {
            notional + self.margin
        }
    }

    /// Unrealized P&L at `mark` (mark-based, doc 08 §7.3).
    pub fn unrealized(&self, mark: f64) -> f64 {
        self.size * (mark - self.entry)
    }

    /// Margin ratio per the Bluefin definition; `None` when flat.
    pub fn margin_ratio(&self, mark: f64) -> Option<f64> {
        if self.size == 0.0 || mark <= 0.0 {
            return None;
        }
        let r = self.debt() / self.balance(mark);
        Some(if self.size > 0.0 { 1.0 - r } else { r - 1.0 })
    }

    /// Mark at which MR = MMR.
    pub fn liquidation_price(&self, mmr: f64) -> Option<f64> {
        if self.size == 0.0 {
            return None;
        }
        let s = self.size.abs();
        Some(if self.size > 0.0 { self.debt() / (s * (1.0 - mmr)) } else { self.debt() / (s * (1.0 + mmr)) })
    }

    /// Mark at which the margin is exhausted (MMR = 0).
    pub fn bankruptcy_price(&self) -> Option<f64> {
        if self.size == 0.0 {
            None
        } else {
            Some(self.debt() / self.size.abs())
        }
    }

    pub fn is_liquidatable(&self, mark: f64, mmr: f64) -> bool {
        self.margin_ratio(mark).is_some_and(|mr| mr < mmr)
    }

    /// Headroom to liquidation as a fraction of the maintenance
    /// requirement: `(MR − MMR) / MMR` (1.0 = one full requirement of
    /// slack, 0 = at the liquidation threshold). `None` when flat.
    pub fn headroom(&self, mark: f64, mmr: f64) -> Option<f64> {
        self.margin_ratio(mark).map(|mr| (mr - mmr) / mmr)
    }
}

/// Margin the desk assigns at `leverage` for `notional`.
pub fn entry_margin(notional: f64, cfg: &MarginConfig) -> f64 {
    notional * cfg.entry_ratio()
}

/// Cash to add so MR returns to the target ratio at `mark`; 0 when the
/// ratio is already above the trigger.
pub fn topup_amount(p: &IsolatedPosition, mark: f64, cfg: &MarginConfig) -> f64 {
    let Some(mr) = p.margin_ratio(mark) else { return 0.0 };
    if mr >= cfg.topup_trigger_mr {
        return 0.0;
    }
    // account value = margin + uPnL = MR × balance
    let balance = p.balance(mark);
    let account = mr * balance;
    (cfg.target_ratio() * balance - account).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Hand check of the documented equations: long 1000 SUI at 3.00 with
    /// 300 margin (10x).
    #[test]
    fn long_ratio_liquidation_and_bankruptcy_match_the_docs() {
        let p = IsolatedPosition { size: 1000.0, entry: 3.0, margin: 300.0 };
        // Debt = 3000 − 300 = 2700; at mark 3.00 balance 3000 → MR = 0.10.
        assert!(near(p.debt(), 2700.0));
        assert!(near(p.margin_ratio(3.0).unwrap(), 0.10));
        // At mark 2.80: balance 2800 → MR = 1 − 2700/2800 = 0.0357…
        assert!(near(p.margin_ratio(2.8).unwrap(), 1.0 - 2700.0 / 2800.0));
        // P_liq = 2700 / (1000 × 0.975) = 2.769…; P_bankruptcy = 2.70.
        let cfg = MarginConfig::default();
        assert!(near(p.liquidation_price(cfg.mmr).unwrap(), 2700.0 / 975.0));
        assert!(near(p.bankruptcy_price().unwrap(), 2.70));
        assert!(!p.is_liquidatable(2.78, cfg.mmr));
        assert!(p.is_liquidatable(2.76, cfg.mmr));
        // Headroom at entry: (0.10 − 0.025)/0.025 = 3.
        assert!(near(p.headroom(3.0, cfg.mmr).unwrap(), 3.0));
        // Unrealized is mark-based.
        assert!(near(p.unrealized(2.8), -200.0));
    }

    /// Short 1000 SUI at 3.00 with 300 margin: debt = 3300; liquidates
    /// when the mark rallies past 3300 / (1000 × 1.025) = 3.219….
    #[test]
    fn short_ratio_and_liquidation_price_match_the_docs() {
        let p = IsolatedPosition { size: -1000.0, entry: 3.0, margin: 300.0 };
        assert!(near(p.debt(), 3300.0));
        assert!(near(p.margin_ratio(3.0).unwrap(), 0.10));
        assert!(near(p.margin_ratio(3.2).unwrap(), 3300.0 / 3200.0 - 1.0));
        let cfg = MarginConfig::default();
        assert!(near(p.liquidation_price(cfg.mmr).unwrap(), 3300.0 / 1025.0));
        assert!(near(p.bankruptcy_price().unwrap(), 3.30));
        assert!(p.is_liquidatable(3.25, cfg.mmr));
        assert!(!p.is_liquidatable(3.21, cfg.mmr));
        assert!(near(p.unrealized(3.2), -200.0));
        assert_eq!(IsolatedPosition { size: 0.0, entry: 0.0, margin: 0.0 }.margin_ratio(3.0), None);
    }

    #[test]
    fn entry_margin_and_topup_restore_the_target_ratio() {
        let cfg = MarginConfig::default();
        assert!(near(cfg.entry_ratio(), 0.10));
        assert!(near(entry_margin(3000.0, &cfg), 300.0));
        // 20x is the cap: leverage 50 still posts IMR.
        let hi = MarginConfig { leverage: 50.0, ..cfg.clone() };
        assert!(near(hi.entry_ratio(), 0.045));
        let p = IsolatedPosition { size: 1000.0, entry: 3.0, margin: 300.0 };
        assert!(near(topup_amount(&p, 3.0, &cfg), 0.0));
        // At 2.80 MR = 0.0357 > trigger 0.035: nothing yet.
        assert!(near(topup_amount(&p, 2.8, &cfg), 0.0));
        // At 2.78 MR = 1 − 2700/2780 = 0.0288 < trigger: top up to 10%
        // of balance 2780 = 278 account value; account is 80 → 198.
        assert!(near(topup_amount(&p, 2.78, &cfg), 278.0 - 80.0));
        assert!(cfg.outages.is_empty() && !cfg.in_outage(5));
        let out = MarginConfig { outages: vec![[10, 20]], ..cfg };
        assert!(out.in_outage(10) && !out.in_outage(20));
    }
}
