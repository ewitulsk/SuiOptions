//! Loader for `solana-deployments.json` — the single source of truth for
//! every on-chain id the Solana off-chain stack needs, plus the off-chain
//! token catalog used for pricing.
//!
//! The Solana twin of `crates/deployments`: same env-slot layout (keyed by
//! deployment environment, un-deployed envs written as `null`), written by
//! `tools/solana-deployment-manager` and read only by solana-token-info —
//! every other service reads through `crates/solana-token-info-client`.
//!
//! ```json
//! {
//!   "dev": null,
//!   "staging": {
//!     "program_info": {
//!       "optionsCoreProgramId":  "6Kei…",
//!       "auctionVenueProgramId": "8cvp…",
//!       "optionsVaultProgramId": "ELxb…",
//!       "configPda":   "…",
//!       "treasuryPda": "…",
//!       "admin":       "…",
//!       "network":     "devnet",
//!       "deployedAt":  "…RFC3339…",
//!       "initializeSignature": "…",
//!       "testTokens": {
//!         "TBTC": { "mint": "…", "decimals": 8, "mintAuthority": "…" }
//!       }
//!     },
//!     "token_info": {
//!       "TBTC": { "mint": "…", "decimals": 8, "pythFeedId": "…64-hex…" }
//!     }
//!   },
//!   "prod": { … }
//! }
//! ```
//!
//! Notes vs the Sui file: program ids are deploy-stable (no upgradeCap /
//! originalPackageId); `configPda` / `treasuryPda` are derivable from the
//! program id but recorded anyway so no consumer needs PDA math; token
//! identity is the **mint address** (base58, byte-exact, no normalization
//! ever); Pyth feed ids stay 64-hex (chain-agnostic). All ids are plain
//! `String`s — no solana-sdk anywhere in the main workspace; use
//! [`validate_pubkey`] / [`SolanaNetworkDeployment::validate`] where a
//! well-formedness check is wanted.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// One SPL test-token mint created by solana-deploy. Mint authority is the
/// gas-station's faucet key (the faucet moved off-chain on Solana).
/// Non-mainnet only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestToken {
    /// SPL mint address (base58).
    pub mint: String,
    pub decimals: u8,
    /// Holder of the mint authority (the gas-station faucet key).
    pub mint_authority: String,
}

/// Off-chain token catalog entry. One per supported ticker; carries
/// everything off-chain pricers need to source a USD spot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSpec {
    /// SPL mint address (base58). On non-mainnet, must match the same
    /// symbol's entry under `program_info.testTokens`.
    pub mint: String,
    pub decimals: u8,
    /// 64-hex Pyth feed id (chain-agnostic). Optional so tokens without a
    /// real-world feed (synthetic test tokens) still appear in the catalog.
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
}

/// On-chain artifacts from deploying + initializing the three programs
/// (options_core / auction_venue / options_vault).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramInfo {
    pub options_core_program_id: String,
    pub auction_venue_program_id: String,
    pub options_vault_program_id: String,
    /// options_core Config PDA — the quote `protocol_id`.
    pub config_pda: String,
    pub treasury_pda: String,
    /// config.admin pubkey.
    pub admin: String,
    /// Solana cluster: `devnet` | `testnet` | `mainnet-beta`.
    pub network: String,
    pub deployed_at: String,
    #[serde(default)]
    pub initialize_signature: Option<String>,
    /// SPL test mints. Non-mainnet only; absent on mainnet-beta.
    #[serde(default)]
    pub test_tokens: Option<BTreeMap<String, TestToken>>,
}

/// Per-env deployment record. `program_info` carries everything derived
/// from deploying the programs; `token_info` is the off-chain pricing
/// catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaNetworkDeployment {
    pub program_info: ProgramInfo,
    #[serde(default)]
    pub token_info: BTreeMap<String, TokenSpec>,
}

impl SolanaNetworkDeployment {
    /// Off-chain catalog lookup (mint, decimals, optional Pyth feed id).
    /// Case-insensitive on the symbol.
    pub fn token_spec(&self, symbol: &str) -> Result<&TokenSpec> {
        let upper = symbol.to_ascii_uppercase();
        self.token_info.get(&upper).ok_or_else(|| {
            anyhow!(
                "no token_info entry for {symbol} (have: {:?})",
                self.token_info.keys().collect::<Vec<_>>()
            )
        })
    }

    /// `Result`-flavored access to the test-token block. Errors on
    /// mainnet-beta where it's absent.
    pub fn test_tokens(&self) -> Result<&BTreeMap<String, TestToken>> {
        self.program_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section in solana-deployments.json"))
    }

    /// `Option`-flavored access — for call sites that already handle "no
    /// test tokens deployed" gracefully.
    pub fn maybe_test_tokens(&self) -> Option<&BTreeMap<String, TestToken>> {
        self.program_info.test_tokens.as_ref()
    }

    /// Case-insensitive test-token lookup.
    pub fn test_token(&self, symbol: &str) -> Result<&TestToken> {
        let tokens = self.test_tokens()?;
        let upper = symbol.to_ascii_uppercase();
        tokens.get(&upper).ok_or_else(|| {
            anyhow!(
                "no test token named {symbol} (have: {:?})",
                tokens.keys().collect::<Vec<_>>()
            )
        })
    }

    /// Check every id in the record is a well-formed 32-byte base58
    /// pubkey. Not enforced at deserialization — call it where corrupt
    /// ids should fail fast (e.g. solana-token-info boot).
    pub fn validate(&self) -> Result<()> {
        let p = &self.program_info;
        validate_pubkey(&p.options_core_program_id, "optionsCoreProgramId")?;
        validate_pubkey(&p.auction_venue_program_id, "auctionVenueProgramId")?;
        validate_pubkey(&p.options_vault_program_id, "optionsVaultProgramId")?;
        validate_pubkey(&p.config_pda, "configPda")?;
        validate_pubkey(&p.treasury_pda, "treasuryPda")?;
        validate_pubkey(&p.admin, "admin")?;
        if let Some(test_tokens) = &p.test_tokens {
            for (sym, t) in test_tokens {
                validate_pubkey(&t.mint, &format!("testTokens.{sym}.mint"))?;
                validate_pubkey(&t.mint_authority, &format!("testTokens.{sym}.mintAuthority"))?;
            }
        }
        for (sym, spec) in &self.token_info {
            validate_pubkey(&spec.mint, &format!("token_info.{sym}.mint"))?;
        }
        Ok(())
    }
}

/// Check `s` base58-decodes to exactly 32 bytes (a Solana pubkey).
pub fn validate_pubkey(s: &str, field: &str) -> Result<()> {
    let bytes = bs58::decode(s)
        .into_vec()
        .with_context(|| format!("base58 decode of {field} ({s:?})"))?;
    if bytes.len() != 32 {
        bail!("{field} ({s:?}) decodes to {} bytes, want 32", bytes.len());
    }
    Ok(())
}

/// All recorded Solana deployments, keyed by environment (`staging` /
/// `prod`). Un-deployed envs are `null` in the file and kept as `None`
/// slots here (the writer round-trips the full shape).
#[derive(Debug)]
pub struct SolanaDeployments {
    pub envs: BTreeMap<String, Option<SolanaNetworkDeployment>>,
}

impl SolanaDeployments {
    pub fn load(path: &Path) -> Result<Self> {
        info!(path = %path.display(), "loading solana deployments");
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading solana deployments file {}", path.display()))?;
        // Keys are lowercased so lookup is case-insensitive.
        let raw: BTreeMap<String, Option<SolanaNetworkDeployment>> =
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing solana deployments file {}", path.display()))?;
        let envs = raw
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect::<BTreeMap<_, _>>();
        debug!(envs = ?envs.keys().collect::<Vec<_>>(), "solana deployments loaded");
        Ok(Self { envs })
    }

    /// Environment slot lookup. Accepts any casing; an env with no
    /// recorded deployment (missing or `null`) errors.
    pub fn for_env(&self, env: &str) -> Result<&SolanaNetworkDeployment> {
        self.envs
            .get(&env.to_ascii_lowercase())
            .and_then(|slot| slot.as_ref())
            .ok_or_else(|| anyhow!("no solana deployment recorded for env {env}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Valid 32-byte base58 pubkeys (program ids from the port plan + the
    // wrapped-SOL mint / system program).
    const CORE: &str = "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t";
    const VENUE: &str = "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk";
    const VAULT: &str = "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe";
    const MINT: &str = "So11111111111111111111111111111111111111112";
    const ADMIN: &str = "11111111111111111111111111111111";

    fn fixture() -> String {
        format!(
            r#"{{
              "dev": null,
              "staging": {{
                "program_info": {{
                  "optionsCoreProgramId": "{CORE}",
                  "auctionVenueProgramId": "{VENUE}",
                  "optionsVaultProgramId": "{VAULT}",
                  "configPda": "{ADMIN}",
                  "treasuryPda": "{ADMIN}",
                  "admin": "{ADMIN}",
                  "network": "devnet",
                  "deployedAt": "2026-07-11T00:00:00Z",
                  "initializeSignature": "sig",
                  "testTokens": {{
                    "TBTC": {{ "mint": "{MINT}", "decimals": 8, "mintAuthority": "{ADMIN}" }}
                  }}
                }},
                "token_info": {{
                  "TBTC": {{ "mint": "{MINT}", "decimals": 8, "pythFeedId": "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43" }},
                  "TUSDC": {{ "mint": "{MINT}", "decimals": 6 }}
                }}
              }},
              "prod": {{
                "program_info": {{
                  "optionsCoreProgramId": "{CORE}",
                  "auctionVenueProgramId": "{VENUE}",
                  "optionsVaultProgramId": "{VAULT}",
                  "configPda": "{ADMIN}",
                  "treasuryPda": "{ADMIN}",
                  "admin": "{ADMIN}",
                  "network": "mainnet-beta",
                  "deployedAt": "2026-07-11T00:00:00Z"
                }},
                "token_info": {{}}
              }}
            }}"#
        )
    }

    fn load_fixture() -> SolanaDeployments {
        // Unique per call — tests run in parallel and must not share files.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "solana-deployments-test-{}-{n}.json",
            std::process::id()
        ));
        std::fs::write(&path, fixture()).unwrap();
        let d = SolanaDeployments::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        d
    }

    #[test]
    fn loads_both_envs_and_null_slot_errors() {
        let d = load_fixture();
        let staging = d.for_env("staging").unwrap();
        assert_eq!(staging.program_info.options_core_program_id, CORE);
        assert_eq!(staging.program_info.network, "devnet");
        assert_eq!(
            staging.program_info.initialize_signature.as_deref(),
            Some("sig")
        );
        // Case-insensitive env lookup.
        assert!(d.for_env("STAGING").is_ok());
        // Populated prod: no testTokens, no initializeSignature.
        let prod = d.for_env("prod").unwrap();
        assert!(prod.program_info.test_tokens.is_none());
        assert!(prod.program_info.initialize_signature.is_none());
        assert!(prod.test_tokens().is_err());
        // dev is a null slot; garbage is absent — both error.
        assert!(d.for_env("dev").is_err());
        assert!(d.for_env("garbage").is_err());
        // The null slot survives the load (writers round-trip the shape).
        assert!(matches!(d.envs.get("dev"), Some(None)));
    }

    #[test]
    fn token_and_test_token_lookups_are_case_insensitive() {
        let d = load_fixture();
        let staging = d.for_env("staging").unwrap();
        assert_eq!(staging.token_spec("tbtc").unwrap().decimals, 8);
        assert!(staging.token_spec("TBTC").unwrap().pyth_feed_id.is_some());
        assert!(staging.token_spec("tusdc").unwrap().pyth_feed_id.is_none());
        assert!(staging.token_spec("nope").is_err());
        let t = staging.test_token("tbtc").unwrap();
        assert_eq!(t.mint, MINT);
        assert_eq!(t.mint_authority, ADMIN);
        assert!(staging.test_token("nope").is_err());
    }

    #[test]
    fn validate_checks_base58_pubkeys() {
        let d = load_fixture();
        d.for_env("staging").unwrap().validate().unwrap();
        d.for_env("prod").unwrap().validate().unwrap();

        let mut bad = d.for_env("staging").unwrap().clone();
        bad.program_info.config_pda = "0xdeadbeef".into(); // not base58
        assert!(bad.validate().is_err());
        bad.program_info.config_pda = "abc".into(); // too short
        assert!(bad.validate().is_err());
    }
}
