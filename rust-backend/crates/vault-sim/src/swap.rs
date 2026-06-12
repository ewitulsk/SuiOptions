//! Slippage model for USDC ↔ underlying conversions (doc 06 §3). One
//! number for now — the DeepBook integration this models is itself a
//! min-out-bounded market order (doc 03 §7.3).

use crate::types::{SettleAmt, UnderlyingAmt};

#[derive(Clone, Copy, Debug)]
pub struct SwapModel {
    pub slippage_bps: u64,
}

impl SwapModel {
    /// Settlement → underlying at `price` (settlement smallest-units per
    /// underlying smallest-unit, scale 0), paying the slippage. Floor: the
    /// venue can only round against the vault.
    pub fn settlement_to_underlying(&self, amount: SettleAmt, price: f64) -> UnderlyingAmt {
        assert!(price > 0.0 && price.is_finite());
        let gross = amount.0 as f64 / price;
        UnderlyingAmt((gross * (1.0 - self.slippage_bps as f64 / 10_000.0)).floor() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_haircuts_the_conversion() {
        let swap = SwapModel { slippage_bps: 50 };
        // 1000 settlement units at price 2.0 → 500 gross → 497.5 → 497.
        assert_eq!(
            swap.settlement_to_underlying(SettleAmt(1_000), 2.0),
            UnderlyingAmt(497)
        );
        let free = SwapModel { slippage_bps: 0 };
        assert_eq!(
            free.settlement_to_underlying(SettleAmt(1_000), 2.0),
            UnderlyingAmt(500)
        );
    }
}
