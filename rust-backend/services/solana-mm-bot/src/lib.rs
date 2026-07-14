//! Library surface for the `solana-mm-bot` binary — the Solana port of
//! `services/mm-bot`.
//!
//! Hosts the clap [`Cli`] type and the pure modules: [`pricing`] (the
//! Black-Scholes brain, ported verbatim), [`auction`] (the unified venue
//! bidder — covered_call / cash_secured_put / swap modes over one
//! `decide_bid`), the WS wire [`messages`] mirror, the minimal
//! [`api_client`] against solana-api-service, and [`bootstrap`] (MmAccount
//! PDA / ATA / inventory plumbing). The async bot loop lives in `main.rs`.

use std::path::PathBuf;

use clap::Parser;

pub mod api_client;
pub mod auction;
pub mod bootstrap;
pub mod coding;
pub mod messages;
pub mod pricing;
pub mod ws_client;

/// True when a bid tx failed only because of a benign venue race —
/// outbid between read and submit (`BidTooLow`), or the deadline crossed
/// mid-flight / the auction was already settled (`AuctionClosed` /
/// `AuctionNotClosed`). These are the expected lost-race outcomes and must
/// not page; every other bid failure fires the
/// `tx-failed-solana-mm-bot-<flow>` alert.
pub fn is_benign_bid_loss(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    let Some(code) = solana_tx::extract_error_code(&text) else {
        return false;
    };
    matches!(
        solana_tx::classify(solana_tx::Program::AuctionVenue, code),
        solana_tx::Classification::Benign
    )
}

#[derive(Parser, Debug)]
#[command(
    name = "solana-mm-bot",
    about = "Market-maker bot for the Solana options protocol"
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/solana-mm-bot/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the solana-token-info service. Resolved at boot; hard
    /// cutover — no solana-deployments.json fallback.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the solana-oracle-service: live prices over its WS
    /// fanout (the single Pyth gateway).
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Base URL of the solana-api-service. The bot resolves each RFQ's
    /// bucket (strike, expiry, mints, option kind) from here by address, so
    /// it never trusts pricing inputs delivered on the RFQ broadcast itself.
    #[arg(long, env = "API_URL", default_value = "http://127.0.0.1:9003")]
    pub api_url: String,

    /// Per-binary secrets TOML. Holds the Solana wallet keypair (under the
    /// network selected by `network` in the bot config) and the ed25519
    /// quote-signing seed (`mm_bot.quote_key`). No env-var fallback.
    #[arg(short = 's', long, default_value = "services/solana-mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-mm-bot",
    cargo_pkg   = "solana-mm-bot",
    working_dir = ".",
    description = "Market-maker bot for the Solana options protocol. First run bootstraps the \
                   MmAccount PDA and deposits inventory; every run authenticates over WS and \
                   prices incoming RFQs with Black-Scholes, and bids the venue's auctions.",
    cli         = crate::Cli,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue_err(code: u32) -> anyhow::Error {
        anyhow::anyhow!(
            "bid failed: RPC response error -32002: Transaction simulation failed: \
             Error processing Instruction 0: custom program error: {:#x}",
            code
        )
    }

    #[test]
    fn bid_too_low_and_auction_closed_are_benign() {
        // BidTooLow = venue variant 2 → 6002; AuctionClosed = 6000.
        assert!(is_benign_bid_loss(&venue_err(6002)));
        assert!(is_benign_bid_loss(&venue_err(6000)));
    }

    #[test]
    fn other_failures_are_not_benign() {
        // WrongSettleAuthority (6007) is a mis-built tx → not benign.
        assert!(!is_benign_bid_loss(&venue_err(6007)));
        assert!(!is_benign_bid_loss(&anyhow::anyhow!("blockhash not found")));
    }

    /// The quote the bot ships must verify against the exact canonical
    /// bytes the on-chain `execute_write` verifier compares (the program
    /// type's Borsh encoding, via solana-tx's golden-tested helpers), and
    /// the wire form must round-trip losslessly.
    #[test]
    fn quote_sign_verify_round_trips_against_solana_tx() {
        use ed25519_dalek::Verifier as _;
        use solana_sdk::pubkey::Pubkey;
        use solana_tx::quote::{quote_bytes, quote_pubkey, sign_quote, Quote, QuoteWire};

        let quote = Quote {
            protocol_id: Pubkey::new_from_array([0x11; 32]),
            signer_account: Pubkey::new_from_array([0x22; 32]),
            signer_token_recipient: Pubkey::new_from_array([0x33; 32]),
            bucket: Pubkey::new_from_array([0x44; 32]),
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_748_534_400_000,
            nonce: 42,
        };
        let seed = crate::bootstrap::parse_quote_seed(&hex::encode([7u8; 32])).unwrap();
        let sig = sign_quote(&seed, &quote).unwrap();
        let pk = quote_pubkey(&seed).unwrap();

        // Detached signature verifies over the canonical Borsh bytes.
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).unwrap();
        let msg = quote_bytes(&quote);
        vk.verify(&msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .unwrap();
        // …and NOT over tampered bytes.
        let tampered = quote_bytes(&Quote { premium: quote.premium + 1, ..quote });
        assert!(vk
            .verify(&tampered, &ed25519_dalek::Signature::from_bytes(&sig))
            .is_err());

        // Wire round-trip: the JSON QuoteWire the bot sends reconstructs the
        // identical program Quote (so the executor signs/verifies the same
        // bytes).
        let wire = QuoteWire::from(&quote);
        let back = Quote::try_from(&wire).unwrap();
        assert_eq!(back, quote);
        assert_eq!(quote_bytes(&back), msg);
    }
}
