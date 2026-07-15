//! Shared Move-package publish machinery.
//!
//! Extracted from tools/deployment-manager so mm-bot's `deploy-collateral`
//! can reuse the same `BuildConfig` + `Published.toml` pipeline: compile a
//! package on disk, publish it, and stamp (or leave alone) its
//! `Published.toml`.
//!
//! `Published.toml` discipline: the package resolver reads a dependency's
//! published address from its own `Published.toml`, and a package being
//! published fresh must compile with no published id of its own — so
//! [`stash_pubfile`] deletes the file before the build and [`finish_pubfile`]
//! writes the fresh id back (preserving other environments' sections) or
//! restores the original on failure. Callers that publish from a throwaway
//! copy of the package dir (mm-bot) simply discard the stamped copy.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_move_build::BuildConfig;
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::SuiKeyPair;
use sui_types::transaction::Transaction;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::SUI_FRAMEWORK_ADDRESS;

pub struct DepPublishOutcome {
    pub package_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
    /// Every non-UpgradeCap object the publish's `init` functions created, as
    /// `(module, struct_name, object_id)` — how callers locate init-created
    /// shared objects (e.g. mm_collateral's `CollateralAccount`).
    pub created_objects: Vec<(String, String, ObjectID)>,
}

/// The stashed original `Published.toml` contents (if any) for a package
/// about to be published fresh.
pub struct PubfileStash {
    original: Option<String>,
}

/// Stash + delete the package's `Published.toml` before a fresh publish.
pub fn stash_pubfile(path: &Path) -> Result<PubfileStash> {
    let pubfile_path = path.join("Published.toml");
    let original = std::fs::read_to_string(&pubfile_path).ok();
    if original.is_some() {
        std::fs::remove_file(&pubfile_path)
            .with_context(|| format!("removing {} for publish", pubfile_path.display()))?;
    }
    Ok(PubfileStash { original })
}

/// Write the fresh id into the package's `Published.toml` (preserving other
/// environments' sections), or restore the stashed original on failure.
pub async fn finish_pubfile(
    client: &SuiClient,
    path: &Path,
    stash: PubfileStash,
    env_name: &str,
    published: Option<ObjectID>,
) -> Result<()> {
    let pubfile_path = path.join("Published.toml");
    match published {
        Some(package_id) => {
            let pkg = package_id.to_hex_uncompressed();
            let chain_id = client
                .read_api()
                .get_chain_identifier()
                .await
                .context("fetching chain identifier for Published.toml")?;
            std::fs::write(
                &pubfile_path,
                pubfile_for_published(stash.original.as_deref(), env_name, &chain_id, &pkg),
            )
            .with_context(|| {
                format!("writing published id into {}", pubfile_path.display())
            })?;
            tracing::info!(
                pubfile = %pubfile_path.display(),
                package = %pkg,
                "publish metadata updated to the published id"
            );
        }
        None => {
            // Best-effort restore so a failed run leaves the tree clean.
            if let Some(original) = &stash.original {
                let _ = std::fs::write(&pubfile_path, original);
            }
        }
    }
    Ok(())
}

/// Compile + publish one Move package and stamp its `Published.toml` so
/// downstream packages compile against the fresh id. `label` is for logs.
pub async fn publish_dep_package(
    client: &SuiClient,
    keypair: &SuiKeyPair,
    sender: SuiAddress,
    path: &Path,
    label: &str,
    env_name: &str,
    gas_budget: u64,
) -> Result<DepPublishOutcome> {
    let stash = stash_pubfile(path)?;
    let result = publish_dep_inner(client, keypair, sender, path, label, gas_budget).await;
    finish_pubfile(client, path, stash, env_name, result.as_ref().ok().map(|o| o.package_id))
        .await?;
    result
}

async fn publish_dep_inner(
    client: &SuiClient,
    keypair: &SuiKeyPair,
    sender: SuiAddress,
    path: &Path,
    label: &str,
    gas_budget: u64,
) -> Result<DepPublishOutcome> {
    tracing::info!(path = %path.display(), package = label, "compiling Move package");
    let compiled = BuildConfig::new_for_testing()
        .build(path)
        .with_context(|| format!("compiling {label} package at {}", path.display()))?;
    let modules = compiled.get_package_bytes(false);
    let deps = compiled.get_dependency_storage_package_ids();

    let tx_data = client
        .transaction_builder()
        .publish(sender, modules, deps, None, gas_budget)
        .await
        .with_context(|| format!("building {label} publish tx"))?;
    let signature =
        Transaction::signature_from_signer(tx_data.clone(), Intent::sui_transaction(), keypair);
    let tx = Transaction::from_data(tx_data, vec![signature]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!(package = label, "submitting publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .with_context(|| format!("submitting {label} publish tx"))?;
    assert_success(&resp)?;

    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("{label} publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;
    let mut created_objects = Vec::new();
    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => package_id = Some(*pid),
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } => {
                if object_type.address == SUI_FRAMEWORK_ADDRESS
                    && object_type.module.as_str() == "package"
                    && object_type.name.as_str() == "UpgradeCap"
                {
                    upgrade_cap_id = Some(*object_id);
                } else {
                    created_objects.push((
                        object_type.module.as_str().to_owned(),
                        object_type.name.as_str().to_owned(),
                        *object_id,
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(DepPublishOutcome {
        package_id: package_id
            .ok_or_else(|| anyhow!("{label} publish: no Published change"))?,
        upgrade_cap_id: upgrade_cap_id
            .ok_or_else(|| anyhow!("{label} publish: no UpgradeCap created"))?,
        digest,
        created_objects,
    })
}

/// `Published.toml` with the `[published.<env>]` section replaced (or
/// appended) to point at the fresh package id. Other environments' sections
/// are preserved verbatim.
pub fn pubfile_for_published(
    original: Option<&str>,
    env_name: &str,
    chain_id: &str,
    package_id: &str,
) -> String {
    let header = format!("[published.{env_name}]");
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for l in original.unwrap_or_default().lines() {
        let t = l.trim();
        if t == header {
            skipping = true;
            continue;
        }
        if skipping {
            if t.starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push(l.to_owned());
    }
    while matches!(out.last(), Some(l) if l.trim().is_empty()) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push(header);
    out.push(format!("chain-id = \"{chain_id}\""));
    out.push(format!("published-at = \"{package_id}\""));
    out.push(format!("original-id = \"{package_id}\""));
    out.push("version = 1".to_owned());
    out.join("\n") + "\n"
}

pub fn assert_success(resp: &SuiTransactionBlockResponse) -> Result<()> {
    let effects = resp
        .effects
        .as_ref()
        .ok_or_else(|| anyhow!("response missing effects"))?;
    if effects.status().is_err() {
        bail!("tx failed: {:?}", effects.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pubfile_for_published;

    #[test]
    fn pubfile_replaces_existing_env_section() {
        let original = "# header comment\n[published.testnet]\nchain-id = \"4c78adac\"\npublished-at = \"0xold\"\noriginal-id = \"0xold\"\nversion = 1\ntoolchain-version = \"1.63.2\"\n\n[published.mainnet]\nchain-id = \"35834a8a\"\npublished-at = \"0xmain\"\noriginal-id = \"0xmain\"\nversion = 1\n";
        let out = pubfile_for_published(Some(original), "testnet", "4c78adac", "0xnew");
        assert!(!out.contains("0xold"));
        assert!(out.contains("[published.mainnet]"));
        assert!(out.contains("0xmain"));
        assert!(out.contains("[published.testnet]\nchain-id = \"4c78adac\"\npublished-at = \"0xnew\"\noriginal-id = \"0xnew\"\nversion = 1\n"));
    }

    #[test]
    fn pubfile_appends_when_missing() {
        let out = pubfile_for_published(None, "testnet", "4c78adac", "0xnew");
        assert_eq!(
            out,
            "[published.testnet]\nchain-id = \"4c78adac\"\npublished-at = \"0xnew\"\noriginal-id = \"0xnew\"\nversion = 1\n"
        );
    }
}
