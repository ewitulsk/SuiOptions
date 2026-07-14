//! Pure decision logic — everything here is testable without an RPC.
//!
//! Solana has no "publish = new identity", so the tool must be re-runnable:
//! every step checks existing state first and converges instead of erroring.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use solana_sdk::pubkey::Pubkey;

use options_core::state::Config;
use solana_deployments::{TestToken, TokenSpec};

/// Test-token table: symbol → decimals. TSOL replaces TSUI as the
/// gas-asset stand-in; TWAL/TDEEP are Sui-only and dropped.
pub const TEST_TOKENS: [(&str, u8); 3] = [("TBTC", 8), ("TSOL", 9), ("TUSDC", 6)];

/// Seed Pyth feed ids for fresh `token_info` entries (64-hex,
/// chain-agnostic). BTC/USDC are the same beta feeds the Sui
/// deployments.json uses; SOL is the standard Pyth SOL/USD feed.
pub fn default_pyth_feed_id(symbol: &str) -> Option<&'static str> {
    match symbol {
        "TBTC" => Some("f9c0172ba10dfa4d19088d94f5bf61d3b54d5bd7483a322a982e1373ee8ea31b"),
        "TUSDC" => Some("41f3625971ca2ed2263e78573fe5ce23e13d2558ed3f2e47ab0f84fb9e7ae722"),
        "TSOL" => Some("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitAction {
    /// Config PDA exists and its admin is our key — nothing to do.
    AlreadyInitialized,
    /// No Config PDA yet — send `initialize`.
    Initialize,
}

/// Idempotent-init decision: given the (possibly absent) on-chain Config,
/// skip when it exists with our admin, error on an admin mismatch.
pub fn plan_initialize(existing: Option<&Config>, our_admin: &Pubkey) -> Result<InitAction> {
    match existing {
        Some(cfg) if cfg.admin == *our_admin => Ok(InitAction::AlreadyInitialized),
        Some(cfg) => bail!(
            "Config PDA already initialized with admin {} but the loaded keypair is {our_admin}; \
             refusing to proceed",
            cfg.admin
        ),
        None => Ok(InitAction::Initialize),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenAction {
    /// The recorded mint exists on-chain with the right decimals — keep it.
    Keep,
    /// Missing record, vanished mint, or wrong decimals — create a fresh mint.
    Create,
}

/// Per-token idempotency: keep the recorded mint only when it still exists
/// on-chain (`on_chain_decimals` is `Some`) with the expected decimals.
pub fn plan_token(
    recorded: Option<&TestToken>,
    on_chain_decimals: Option<u8>,
    want_decimals: u8,
) -> TokenAction {
    match (recorded, on_chain_decimals) {
        (Some(rec), Some(d)) if rec.decimals == want_decimals && d == want_decimals => {
            TokenAction::Keep
        }
        _ => TokenAction::Create,
    }
}

/// Rebuild the `token_info` catalog from this run's test tokens, with the
/// same carry-forward discipline as the Sui tool: an existing entry's
/// `pythFeedId` always wins; fresh entries get the seeded default. Entries
/// for symbols outside the test-token set are preserved untouched.
pub fn build_token_info(
    previous: BTreeMap<String, TokenSpec>,
    test_tokens: &BTreeMap<String, TestToken>,
) -> BTreeMap<String, TokenSpec> {
    let mut out = previous;
    for (sym, rec) in test_tokens {
        let pyth = out
            .get(sym)
            .and_then(|s| s.pyth_feed_id.clone())
            .or_else(|| default_pyth_feed_id(sym).map(str::to_owned));
        out.insert(
            sym.clone(),
            TokenSpec {
                mint: rec.mint.clone(),
                decimals: rec.decimals,
                pyth_feed_id: pyth,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn config(admin: Pubkey) -> Config {
        Config {
            admin,
            fee_bps: 0,
            bump: 255,
        }
    }

    fn test_token(mint: Pubkey, decimals: u8) -> TestToken {
        TestToken {
            mint: mint.to_string(),
            decimals,
            mint_authority: pk(9).to_string(),
        }
    }

    #[test]
    fn init_plan_is_idempotent() {
        let admin = pk(1);
        assert_eq!(
            plan_initialize(None, &admin).unwrap(),
            InitAction::Initialize
        );
        assert_eq!(
            plan_initialize(Some(&config(admin)), &admin).unwrap(),
            InitAction::AlreadyInitialized
        );
        // Foreign admin → hard error, never a silent overwrite.
        assert!(plan_initialize(Some(&config(pk(2))), &admin).is_err());
    }

    #[test]
    fn token_plan_keeps_only_live_matching_mints() {
        let rec = test_token(pk(3), 6);
        // Recorded + on-chain + right decimals → keep.
        assert_eq!(plan_token(Some(&rec), Some(6), 6), TokenAction::Keep);
        // Never recorded → create.
        assert_eq!(plan_token(None, None, 6), TokenAction::Create);
        // Recorded but vanished on-chain → create.
        assert_eq!(plan_token(Some(&rec), None, 6), TokenAction::Create);
        // On-chain decimals drifted → create.
        assert_eq!(plan_token(Some(&rec), Some(8), 6), TokenAction::Create);
        // Table decimals changed since the record was written → create.
        assert_eq!(plan_token(Some(&rec), Some(6), 9), TokenAction::Create);
    }

    #[test]
    fn token_info_carries_pyth_feed_ids_forward() {
        let mut previous = BTreeMap::new();
        previous.insert(
            "TBTC".to_owned(),
            TokenSpec {
                mint: pk(4).to_string(),
                decimals: 8,
                pyth_feed_id: Some("operator-set-feed".to_owned()),
            },
        );
        // A symbol outside the test-token table must survive untouched.
        previous.insert(
            "REAL".to_owned(),
            TokenSpec {
                mint: pk(5).to_string(),
                decimals: 9,
                pyth_feed_id: None,
            },
        );

        let mut fresh = BTreeMap::new();
        fresh.insert("TBTC".to_owned(), test_token(pk(6), 8)); // rebuilt mint
        fresh.insert("TSOL".to_owned(), test_token(pk(7), 9)); // brand new

        let out = build_token_info(previous, &fresh);
        // Existing pythFeedId wins over the seeded default; mint updates.
        let tbtc = &out["TBTC"];
        assert_eq!(tbtc.pyth_feed_id.as_deref(), Some("operator-set-feed"));
        assert_eq!(tbtc.mint, pk(6).to_string());
        // Fresh entry gets the seeded default.
        assert_eq!(
            out["TSOL"].pyth_feed_id.as_deref(),
            default_pyth_feed_id("TSOL")
        );
        // Unrelated entry preserved.
        assert!(out.contains_key("REAL"));
    }

    /// PDA derivation drift tripwire: the configPda/treasuryPda we record
    /// via solana-tx must match a raw derivation from the program crate's
    /// own seed constants.
    #[test]
    fn recorded_pdas_match_program_seeds() {
        let core = options_core::ID;
        assert_eq!(
            solana_tx::pda::config(&core),
            Pubkey::find_program_address(&[options_core::state::CONFIG_SEED], &core).0
        );
        assert_eq!(
            solana_tx::pda::treasury(&core),
            Pubkey::find_program_address(&[options_core::state::TREASURY_SEED], &core).0
        );
        assert_ne!(solana_tx::pda::config(&core), solana_tx::pda::treasury(&core));
    }
}
