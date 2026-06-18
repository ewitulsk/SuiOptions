//! Predicted-APY math + market-data resolution, folded in from the retired
//! derived-metric-worker (SO). `compute` is pure (fed resolved inputs);
//! `spot` resolves the Pyth USD cross + realized-vol feed id. The sampler
//! that drives these lives in `crate::apy_sampler`.

pub mod compute;
pub mod spot;
