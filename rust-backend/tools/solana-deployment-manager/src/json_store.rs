//! Read-merge-write store for `solana-deployments.json`, mirroring the Sui
//! tool's `json_store` discipline: env-slot upsert, other envs untouched,
//! un-deployed envs rendered as `null` so the on-disk shape is stable.
//!
//! The record type is the reader crate's
//! [`solana_deployments::SolanaNetworkDeployment`] — writer and reader
//! share one serde definition, so the shapes can't drift (snake_case
//! containers `program_info` / `token_info`, camelCase fields inside).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use solana_deployments::SolanaNetworkDeployment;

/// The deployment environments we always render as keys (even when unset),
/// so humans can see what's missing at a glance.
const ENVS: [&str; 3] = ["dev", "prod", "staging"];

#[derive(Debug, Default)]
pub struct Deployments {
    pub envs: BTreeMap<String, SolanaNetworkDeployment>,
}

impl Deployments {
    /// Reads the file if it exists; returns an empty store if not.
    /// Tolerates `null` entries for un-deployed envs (the shape we
    /// ourselves write on save).
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;

        let mut envs = BTreeMap::new();
        for (key, value) in raw {
            if value.is_null() {
                continue;
            }
            let record: SolanaNetworkDeployment = serde_json::from_value(value)
                .with_context(|| format!("parsing {} entry in {}", key, path.display()))?;
            envs.insert(key, record);
        }
        Ok(Self { envs })
    }

    pub fn upsert(&mut self, env: &str, deployment: SolanaNetworkDeployment) {
        self.envs.insert(env.to_owned(), deployment);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        // Always include every env key, even if unset. Extra keys already
        // in the map are preserved too.
        let mut full = serde_json::Map::new();
        for env in ENVS {
            full.insert(env.to_owned(), serde_json::Value::Null);
        }
        for (env, dep) in &self.envs {
            full.insert(env.clone(), serde_json::to_value(dep)?);
        }
        let pretty = serde_json::to_vec_pretty(&serde_json::Value::Object(full))?;
        std::fs::write(path, pretty).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_deployments::{ProgramInfo, SolanaDeployments, TestToken, TokenSpec};

    const CORE: &str = "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t";
    const VENUE: &str = "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk";
    const VAULT: &str = "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe";
    const MINT: &str = "So11111111111111111111111111111111111111112";
    const ADMIN: &str = "11111111111111111111111111111111";

    fn record(network: &str) -> SolanaNetworkDeployment {
        let mut test_tokens = BTreeMap::new();
        test_tokens.insert(
            "TBTC".to_owned(),
            TestToken {
                mint: MINT.to_owned(),
                decimals: 8,
                mint_authority: ADMIN.to_owned(),
            },
        );
        let mut token_info = BTreeMap::new();
        token_info.insert(
            "TBTC".to_owned(),
            TokenSpec {
                mint: MINT.to_owned(),
                decimals: 8,
                pyth_feed_id: Some(
                    "f9c0172ba10dfa4d19088d94f5bf61d3b54d5bd7483a322a982e1373ee8ea31b".to_owned(),
                ),
            },
        );
        SolanaNetworkDeployment {
            program_info: ProgramInfo {
                options_core_program_id: CORE.to_owned(),
                auction_venue_program_id: VENUE.to_owned(),
                options_vault_program_id: VAULT.to_owned(),
                config_pda: ADMIN.to_owned(),
                treasury_pda: ADMIN.to_owned(),
                admin: ADMIN.to_owned(),
                network: network.to_owned(),
                deployed_at: "2026-07-12T00:00:00+00:00".to_owned(),
                initialize_signature: Some("sig".to_owned()),
                test_tokens: Some(test_tokens),
            },
            token_info,
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "solana-deploy-store-{tag}-{}-{n}.json",
            std::process::id()
        ))
    }

    /// The file we write is exactly what the reader crate parses — the
    /// round-trip test the reader's expectations demand.
    #[test]
    fn round_trips_through_the_reader_crate() {
        let path = temp_path("roundtrip");
        let mut store = Deployments::default();
        store.upsert("staging", record("devnet"));
        store.save(&path).unwrap();

        let read = SolanaDeployments::load(&path).unwrap();
        let staging = read.for_env("staging").unwrap();
        staging.validate().unwrap();
        assert_eq!(staging.program_info.options_core_program_id, CORE);
        assert_eq!(staging.program_info.network, "devnet");
        assert_eq!(
            staging.program_info.initialize_signature.as_deref(),
            Some("sig")
        );
        assert_eq!(staging.test_token("tbtc").unwrap().decimals, 8);
        assert!(staging.token_spec("TBTC").unwrap().pyth_feed_id.is_some());
        // Un-deployed envs are rendered as null slots and survive the load.
        assert!(matches!(read.envs.get("dev"), Some(None)));
        assert!(matches!(read.envs.get("prod"), Some(None)));

        let _ = std::fs::remove_file(&path);
    }

    /// Upserting one env preserves every other env byte-for-byte.
    #[test]
    fn upsert_preserves_other_envs() {
        let path = temp_path("upsert");
        let mut store = Deployments::default();
        store.upsert("staging", record("devnet"));
        store.upsert("prod", record("mainnet-beta"));
        store.save(&path).unwrap();

        let mut reloaded = Deployments::load_or_default(&path).unwrap();
        let mut fresh = record("devnet");
        fresh.program_info.initialize_signature = Some("sig-2".to_owned());
        reloaded.upsert("staging", fresh);
        reloaded.save(&path).unwrap();

        let read = SolanaDeployments::load(&path).unwrap();
        assert_eq!(
            read.for_env("staging")
                .unwrap()
                .program_info
                .initialize_signature
                .as_deref(),
            Some("sig-2")
        );
        // prod untouched.
        let prod = read.for_env("prod").unwrap();
        assert_eq!(prod.program_info.network, "mainnet-beta");
        assert_eq!(prod.program_info.initialize_signature.as_deref(), Some("sig"));

        let _ = std::fs::remove_file(&path);
    }

    /// Missing file → empty store (first deploy bootstrap).
    #[test]
    fn load_or_default_tolerates_missing_file() {
        let store = Deployments::load_or_default(Path::new("/nonexistent/nope.json")).unwrap();
        assert!(store.envs.is_empty());
    }
}
