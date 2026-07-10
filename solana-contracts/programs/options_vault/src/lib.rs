//! Covered-call vault — audit package 3.
//!
//! Phase 3 of the port plan (docs/solana/solana-port-plan.md §5.3). Built on
//! `options_core` and `auction_venue` via CPI; owns the Pyth oracle module.
//! Skeleton until Phase 3.

use anchor_lang::prelude::*;

declare_id!("ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe");

#[program]
pub mod options_vault {}
