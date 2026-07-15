//! The MM's own collateral deployment (plan §8: "deploy, don't create").
//!
//! Core holds no MM funds — each market maker publishes its own copy of the
//! `contracts/mm-collateral` package, whose `init` creates and shares a
//! single `CollateralAccount` owned by the publisher. [`deploy`] compiles
//! that template against the env's published `options_core` and publishes it
//! from a TEMP COPY of the package dir, so the repo tree (in particular the
//! template's `Published.toml`) is never stamped with one MM's package id.
//!
//! The resulting `{package_id, account_id, upgrade_cap}` is persisted to a
//! small TOML state file that serve mode reads (unless the bot config pins
//! `collateral_package` / `collateral_account` explicitly).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;

use sui_tx::sui_client::Signer;

/// Persisted record of one MM's mm_collateral publish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollateralDeployment {
    pub network: String,
    /// The published mm_collateral package (the quote's `release_package`).
    pub package_id: String,
    /// The shared `CollateralAccount` created by `init`
    /// (the quote's `collateral_source`).
    pub account_id: String,
    pub upgrade_cap: String,
}

/// Default state-file path for `network`.
pub fn default_state_path(network: &str) -> PathBuf {
    PathBuf::from(format!("services/mm-bot/config/collateral.{network}.toml"))
}

pub fn load(path: &Path) -> Result<CollateralDeployment> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading collateral state {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing collateral state {}", path.display()))
}

pub fn store(path: &Path, dep: &CollateralDeployment) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(dep).context("encoding collateral state")?;
    std::fs::write(path, raw)
        .with_context(|| format!("writing collateral state {}", path.display()))?;
    Ok(())
}

/// Compile + publish the mm-collateral template and return the deployment
/// record. `contracts` is the template dir in the repo
/// (`contracts/mm-collateral`); `network` names the Published.toml env slot
/// and the state file.
pub async fn deploy(
    client: &SuiClient,
    signer: &Signer,
    contracts: &Path,
    network: &str,
    gas_budget: u64,
) -> Result<CollateralDeployment> {
    let staged = stage_package_copy(contracts)?;
    // The temp copy is discarded whole — its stamped Published.toml with it.
    let result = move_publish::publish_dep_package(
        client,
        &signer.keypair,
        signer.address,
        &staged.dir,
        "mm_collateral",
        network,
        gas_budget,
    )
    .await;
    let outcome = result?;

    let account_id = outcome
        .created_objects
        .iter()
        .find(|(module, name, _)| module == "mm_collateral" && name == "CollateralAccount")
        .map(|(_, _, id)| *id)
        .ok_or_else(|| anyhow!("publish created no CollateralAccount (init not run?)"))?;

    tracing::info!(
        package_id = %outcome.package_id,
        %account_id,
        upgrade_cap = %outcome.upgrade_cap_id,
        digest = %outcome.digest,
        "mm_collateral published"
    );
    Ok(CollateralDeployment {
        network: network.to_string(),
        package_id: outcome.package_id.to_hex_uncompressed(),
        account_id: account_id.to_hex_uncompressed(),
        upgrade_cap: outcome.upgrade_cap_id.to_hex_uncompressed(),
    })
}

/// A temp copy of the package that is deleted on drop.
struct StagedPackage {
    dir: PathBuf,
}

impl Drop for StagedPackage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Copy `Move.toml` + `sources/` into a fresh temp dir, rewriting the
/// relative `options_core = { local = "../core" }` dependency to the
/// absolute path of the repo's `contracts/core` so the copy still compiles
/// against the env's published options_core (via core's own Published.toml,
/// which is read in place and never modified).
fn stage_package_copy(contracts: &Path) -> Result<StagedPackage> {
    let contracts = contracts
        .canonicalize()
        .with_context(|| format!("resolving {}", contracts.display()))?;
    let core = contracts
        .parent()
        .map(|p| p.join("core"))
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            anyhow!(
                "cannot locate contracts/core next to {} (needed for the options_core dep)",
                contracts.display()
            )
        })?;

    let dir = std::env::temp_dir().join(format!(
        "mm-collateral-publish-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("sources")).context("creating temp package dir")?;
    let staged = StagedPackage { dir };

    let manifest = std::fs::read_to_string(contracts.join("Move.toml"))
        .with_context(|| format!("reading {}/Move.toml", contracts.display()))?;
    let rewritten = manifest.replace("\"../core\"", &format!("{:?}", core.to_string_lossy()));
    if rewritten == manifest {
        anyhow::bail!(
            "Move.toml at {} has no `\"../core\"` dependency path to rewrite",
            contracts.display()
        );
    }
    std::fs::write(staged.dir.join("Move.toml"), rewritten)
        .context("writing staged Move.toml")?;

    for entry in std::fs::read_dir(contracts.join("sources")).context("listing sources/")? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) == Some("move") {
            std::fs::copy(entry.path(), staged.dir.join("sources").join(entry.file_name()))
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(staged)
}

/// Resolve the collateral routing for serve mode: explicit config values win,
/// else the persisted state file from `deploy-collateral`.
pub fn resolve(
    config_package: Option<&str>,
    config_account: Option<&str>,
    state_path: &Path,
    network: &str,
) -> Result<(ObjectID, ObjectID)> {
    if let (Some(pkg), Some(acct)) = (config_package, config_account) {
        return Ok((
            ObjectID::from_hex_literal(pkg).context("parsing collateral_package")?,
            ObjectID::from_hex_literal(acct).context("parsing collateral_account")?,
        ));
    }
    let dep = load(state_path).with_context(|| {
        format!(
            "no collateral deployment: set collateral_package/collateral_account in the bot \
             config, or run `mm-bot deploy-collateral` (expected state at {})",
            state_path.display()
        )
    })?;
    if dep.network != network {
        anyhow::bail!(
            "collateral state {} is for network {} but the bot runs on {network}",
            state_path.display(),
            dep.network
        );
    }
    Ok((
        ObjectID::from_hex_literal(&dep.package_id).context("parsing persisted package_id")?,
        ObjectID::from_hex_literal(&dep.account_id).context("parsing persisted account_id")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("mm-collateral-test-{}", std::process::id()));
        let path = dir.join("collateral.staging.toml");
        let dep = CollateralDeployment {
            network: "staging".into(),
            package_id: "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
            account_id: "0x2222222222222222222222222222222222222222222222222222222222222222".into(),
            upgrade_cap: "0x3333333333333333333333333333333333333333333333333333333333333333".into(),
        };
        store(&path, &dep).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.package_id, dep.package_id);
        assert_eq!(back.account_id, dep.account_id);
        let (pkg, acct) = resolve(None, None, &path, "staging").unwrap();
        assert_eq!(pkg.to_hex_uncompressed(), dep.package_id);
        assert_eq!(acct.to_hex_uncompressed(), dep.account_id);
        // Wrong network is refused.
        assert!(resolve(None, None, &path, "prod").is_err());
        // Explicit config wins without touching the file.
        let (pkg, _) = resolve(
            Some("0xaa"),
            Some("0xbb"),
            Path::new("/nonexistent"),
            "staging",
        )
        .unwrap();
        assert_eq!(pkg, ObjectID::from_hex_literal("0xaa").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
