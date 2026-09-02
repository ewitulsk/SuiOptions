//! Library surface for the `vault-messenger` binary.
//!
//! Multichain trading-vault message relay (docs/multichain-vault-plan.md
//! §8): an **initiator and fee payer, not a trust gate**. Watches the EVM
//! spoke chain for outbound `SpokeVault` messages and the Sui hub for
//! `OutboundMessage` events, persists each into a per-lane ordered queue,
//! delivers spoke→hub messages in sequence order through hub PTBs
//! (`endpoint_relayer::deliver` → the matching `multichain` handler →
//! `endpoint_relayer::send`), submits hub→spoke bytes to the spoke's
//! `RelayerEndpoint.deliver`, cranks the spoke's permissionless
//! `syncState()` and the hub's `build_config_sync` on intervals, and
//! alerts on stalls, aged payables, and a low fee pot.

use std::path::PathBuf;

use clap::Parser;

pub mod alerts;
pub mod config;
pub mod cranks;
pub mod db;
pub mod deliverer;
pub mod engine;
pub mod evm;
pub mod hub;
pub mod router;
pub mod state;
pub mod watcher;

#[derive(Parser, Debug)]
#[command(name = "vault-messenger", about = "Hub<->spoke message relay for the multichain trading vault")]
pub struct Cli {
    #[arg(short, long, default_value = "services/vault-messenger/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML with the relayer keys (`[sui]` + `[evm]`) rendered by
    /// render-secrets.sh. Required — the messenger cannot submit without
    /// keys on both chains.
    #[arg(long)]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "vault-messenger",
    cargo_pkg   = "vault-messenger",
    working_dir = ".",
    description = "Relays multichain trading-vault messages between the Sui hub and the EVM \
                   spoke: ordered per-lane delivery, state/config sync cranks, and queue/fee \
                   alerting.",
    cli         = crate::Cli,
}
