//! One MM's `mm_collateral` deployment (collateral abstraction, plan §8).
//!
//! Core holds no MM funds — each market maker publishes its own copy of the
//! `contracts/mm-collateral` template, whose `init` creates and shares a
//! single `CollateralAccount` owned by the publisher. [`deploy`] compiles the
//! template against the env's published `options_core` and publishes it from
//! a TEMP COPY of the package dir, so the repo tree (in particular the
//! template's `Published.toml`) is never stamped with one MM's package id.
//!
//! Lives here (not in mm-bot) because two binaries publish it: the bot's
//! `deploy-collateral` subcommand (standalone MMs / local dev) and the
//! deployment-manager's `--deploy-mm-collateral` (the redeploy-contract
//! workflow, which must re-publish after every options_core republish — the
//! template depends on core by local path, so a fresh core orphans the
//! previous collateral package).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sui_sdk::SuiClient;
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;

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
/// and the state file. Signs with `keypair` — the created `CollateralAccount`
/// is owned by `sender`, so this MUST be the key the bot serves with.
pub async fn deploy(
    client: &SuiClient,
    keypair: &SuiKeyPair,
    sender: SuiAddress,
    contracts: &Path,
    network: &str,
    gas_budget: u64,
) -> Result<CollateralDeployment> {
    let staged = stage_package_copy(contracts)?;
    // The temp copy is discarded whole — its stamped Published.toml with it.
    let result = crate::publish_dep_package(
        client,
        keypair,
        sender,
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
