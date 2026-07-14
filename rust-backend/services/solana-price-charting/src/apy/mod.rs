//! Predicted-APY math + market-data resolution, ported from the Sui twin.
//! `compute` is pure (fed resolved inputs); `spot` resolves the Pyth USD
//! cross via solana-oracle-service. The sampler that drives these lives in
//! `crate::apy_sampler`.

pub mod compute;
pub mod spot;
