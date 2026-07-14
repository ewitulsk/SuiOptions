//! Solana chain plumbing shared by every tx-submitting service — the port
//! of `crates/sui-tx`. Keypair loading from secrets, RPC wrapper,
//! PDA derivations, instruction builders over the real program crates,
//! ed25519-precompile quote helpers, Pyth receiver posting, and Anchor
//! error-code classification.

pub mod client;
pub mod errors;
pub mod ix;
pub mod network;
pub mod pda;
pub mod pyth;
pub mod quote;
pub mod signer;

pub use client::SolanaClientWrapper;
pub use errors::{classify, extract_error_code, Classification, Program};
pub use network::Network;
pub use signer::Signer;
