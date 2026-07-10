//! Generic escrowed ascending-auction venue — audit package 2.
//!
//! Phase 2 of the port plan (docs/solana/solana-port-plan.md §5.2). The
//! generic auction machinery (create/bid/finalize, pure-swap settle) has no
//! dependency on `options_core`; option-settle adapters CPI its
//! `write_collateralized` surface. Skeleton until Phase 2.

use anchor_lang::prelude::*;

declare_id!("8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk");

#[program]
pub mod auction_venue {}
