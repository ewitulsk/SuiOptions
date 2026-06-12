use anyhow::{anyhow, bail, Context, Result};
use shared_crypto::intent::Intent;
use std::path::Path;
use std::time::Duration;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_move_build::BuildConfig;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::transaction::Transaction;
use sui_types::SUI_FRAMEWORK_ADDRESS;

use crate::signer::Signer;

/// Parsed object IDs from the publish transaction.
pub struct PublishOutcome {
    pub package_id: ObjectID,
    pub admin_cap_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
}

/// Build the Move package on disk, publish it via the SDK transaction builder
/// (auto-selects gas), and pull the IDs we care about out of the response.
pub async fn publish_package(
    client: &SuiClient,
    signer: &Signer,
    contracts_path: &Path,
    gas_budget: u64,
) -> Result<PublishOutcome> {
    tracing::info!(path = %contracts_path.display(), "compiling Move package");
    let compiled = BuildConfig::new_for_testing()
        .build(contracts_path)
        .with_context(|| {
            format!("compiling Move package at {}", contracts_path.display())
        })?;

    let modules = compiled.get_package_bytes(/* with_unpublished_deps */ false);
    let deps = compiled.get_dependency_storage_package_ids();
    tracing::info!(modules = modules.len(), deps = deps.len(), "compiled");

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building publish tx")?;

    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);

    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting publish tx")?;

    assert_success(&resp)?;
    extract_publish_outcome(&resp)
}

fn extract_publish_outcome(resp: &SuiTransactionBlockResponse) -> Result<PublishOutcome> {
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut admin_cap_id: Option<ObjectID> = None;
    let mut protocol_config_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;

    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => {
                package_id = Some(*pid);
            }
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } => {
                // 0x2::package::UpgradeCap is created for every publish.
                if object_type.address == SUI_FRAMEWORK_ADDRESS
                    && object_type.module.as_str() == "package"
                    && object_type.name.as_str() == "UpgradeCap"
                {
                    upgrade_cap_id = Some(*object_id);
                    continue;
                }
                // Match by module + struct name; the address is the freshly
                // published package, which we may not have captured yet.
                match (object_type.module.as_str(), object_type.name.as_str()) {
                    ("admin", "AdminCap") => admin_cap_id = Some(*object_id),
                    ("admin", "ProtocolConfig") => protocol_config_id = Some(*object_id),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(PublishOutcome {
        package_id: package_id.ok_or_else(|| anyhow!("no Published object_change in response"))?,
        admin_cap_id: admin_cap_id
            .ok_or_else(|| anyhow!("AdminCap not found in object_changes"))?,
        protocol_config_id: protocol_config_id
            .ok_or_else(|| anyhow!("ProtocolConfig not found in object_changes"))?,
        upgrade_cap_id: upgrade_cap_id
            .ok_or_else(|| anyhow!("UpgradeCap not found in object_changes"))?,
        digest,
    })
}

pub struct InitOutcome {
    pub treasury_id: ObjectID,
    pub digest: String,
}

/// Call `treasury::create_and_share(&AdminCap)`. Idempotent on the read side
/// (it always creates a new Treasury), so callers must avoid double-running
/// this against the same deployment if they want a single canonical treasury.
pub async fn create_and_share_treasury(
    client: &SuiClient,
    signer: &Signer,
    package_id: ObjectID,
    admin_cap_id: ObjectID,
    gas_budget: u64,
) -> Result<InitOutcome> {
    // Give the fullnode a beat to index the freshly created AdminCap before
    // the high-level builder tries to fetch its current version/digest.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let admin_cap_arg = sui_json::SuiJsonValue::from_object_id(admin_cap_id);

    let tx_data = client
        .transaction_builder()
        .move_call(
            signer.address,
            package_id,
            "treasury",
            "create_and_share",
            vec![],
            vec![admin_cap_arg],
            None,
            gas_budget,
            None,
        )
        .await
        .context("building treasury::create_and_share tx")?;

    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);

    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting treasury init tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting treasury init tx")?;

    assert_success(&resp)?;
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("init response missing object_changes"))?;

    let treasury_id = changes
        .iter()
        .find_map(|c| match c {
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "treasury"
                && object_type.name.as_str() == "Treasury" =>
            {
                Some(*object_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("Treasury object not found in init response"))?;

    Ok(InitOutcome { treasury_id, digest })
}

pub struct SessionOutcome {
    pub package_id: ObjectID,
    pub registry_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
}

/// Publish the siws_session package and point its publish metadata at the
/// fresh id.
///
/// The protocol package depends on siws_session through a local Move.toml
/// dependency. The package resolver reads a dependency's published address
/// from its `Published.toml` (falling back to the legacy `published-at` /
/// `[addresses]` manifest fields), so both must be absent for the publish
/// build to compile at `0x0`, and both are rewritten to the fresh id for the
/// protocol build that follows:
///   1. set `siws_session = "0x0"`, drop `published-at`, delete
///      `Published.toml`; build + publish
///   2. on success, write the fresh id into the manifest AND a fresh
///      `Published.toml` entry for this environment
///   3. on failure, restore both files
pub async fn publish_session_package(
    client: &SuiClient,
    signer: &Signer,
    session_path: &Path,
    env_name: &str,
    gas_budget: u64,
) -> Result<SessionOutcome> {
    let manifest_path = session_path.join("Move.toml");
    let pubfile_path = session_path.join("Published.toml");
    let original = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let original_pubfile = std::fs::read_to_string(&pubfile_path).ok();

    std::fs::write(&manifest_path, manifest_for_publish(&original))
        .with_context(|| format!("rewriting {} for publish", manifest_path.display()))?;
    if original_pubfile.is_some() {
        std::fs::remove_file(&pubfile_path)
            .with_context(|| format!("removing {} for publish", pubfile_path.display()))?;
    }

    let result = publish_session_inner(client, signer, session_path, gas_budget).await;

    match &result {
        Ok(outcome) => {
            let pkg = outcome.package_id.to_hex_uncompressed();
            std::fs::write(&manifest_path, manifest_for_published(&original, &pkg))
                .with_context(|| {
                    format!("writing published id into {}", manifest_path.display())
                })?;
            let chain_id = client
                .read_api()
                .get_chain_identifier()
                .await
                .context("fetching chain identifier for Published.toml")?;
            std::fs::write(
                &pubfile_path,
                pubfile_for_published(
                    original_pubfile.as_deref(),
                    env_name,
                    &chain_id,
                    &pkg,
                ),
            )
            .with_context(|| {
                format!("writing published id into {}", pubfile_path.display())
            })?;
            tracing::info!(
                manifest = %manifest_path.display(),
                pubfile = %pubfile_path.display(),
                package = %pkg,
                "session publish metadata updated to the published id"
            );
        }
        Err(_) => {
            // Best-effort restore so a failed run leaves the tree clean.
            let _ = std::fs::write(&manifest_path, &original);
            if let Some(pubfile) = &original_pubfile {
                let _ = std::fs::write(&pubfile_path, pubfile);
            }
        }
    }
    result
}

/// `[addresses] siws_session` zeroed + `published-at` dropped.
fn manifest_for_publish(original: &str) -> String {
    original
        .lines()
        .filter(|l| !l.trim_start().starts_with("published-at"))
        .map(|l| {
            if l.trim_start().starts_with("siws_session = \"") {
                "siws_session = \"0x0\"".to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `[addresses] siws_session` + `published-at` set to the fresh package id.
fn manifest_for_published(original: &str, package_id: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut wrote_published_at = false;
    for l in original.lines() {
        let t = l.trim_start();
        if t.starts_with("published-at") {
            out.push(format!("published-at = \"{package_id}\""));
            wrote_published_at = true;
        } else if t.starts_with("siws_session = \"") {
            out.push(format!("siws_session = \"{package_id}\""));
        } else {
            out.push(l.to_owned());
        }
    }
    if !wrote_published_at {
        // Keep `published-at` adjacent to the [package] header block.
        if let Some(pos) = out.iter().position(|l| l.trim() == "[dependencies]") {
            out.insert(pos, format!("published-at = \"{package_id}\""));
            out.insert(pos + 1, String::new());
        } else {
            out.push(format!("published-at = \"{package_id}\""));
        }
    }
    out.join("\n") + "\n"
}

/// `Published.toml` with the `[published.<env>]` section replaced (or
/// appended) to point at the fresh package id. Other environments' sections
/// are preserved verbatim.
fn pubfile_for_published(
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

async fn publish_session_inner(
    client: &SuiClient,
    signer: &Signer,
    session_path: &Path,
    gas_budget: u64,
) -> Result<SessionOutcome> {
    tracing::info!(path = %session_path.display(), "compiling siws_session package");
    let compiled = BuildConfig::new_for_testing()
        .build(session_path)
        .with_context(|| {
            format!("compiling siws_session package at {}", session_path.display())
        })?;
    let modules = compiled.get_package_bytes(false);
    let deps = compiled.get_dependency_storage_package_ids();

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building siws_session publish tx")?;
    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting siws_session publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting siws_session publish tx")?;
    assert_success(&resp)?;

    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("siws_session publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut registry_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;
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
                } else if object_type.module.as_str() == "registry"
                    && object_type.name.as_str() == "Registry"
                {
                    registry_id = Some(*object_id);
                }
            }
            _ => {}
        }
    }

    Ok(SessionOutcome {
        package_id: package_id
            .ok_or_else(|| anyhow!("siws_session publish: no Published change"))?,
        registry_id: registry_id
            .ok_or_else(|| anyhow!("siws_session publish: no Registry created"))?,
        upgrade_cap_id: upgrade_cap_id
            .ok_or_else(|| anyhow!("siws_session publish: no UpgradeCap created"))?,
        digest,
    })
}

/// Symbol → (module name, decimals). Hardcoded because Move modules name
/// their OTW after the module in uppercase, so module="tusdc" implies
/// type="TUSDC". Decimals match the real-world tokens they shadow.
const TEST_TOKEN_TABLE: &[(&str, &str, u8)] = &[
    ("TUSDC", "tusdc", 6),
    ("TBTC", "tbtc", 8),
    ("TWAL", "twal", 9),
    ("TDEEP", "tdeep", 6),
];

pub struct TestTokenInfo {
    pub symbol: String,
    pub coin_type: String,
    pub faucet_id: ObjectID,
    pub decimals: u8,
}

pub struct TestTokensOutcome {
    pub package_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
    pub tokens: Vec<TestTokenInfo>,
}

/// Compile + publish the test-tokens package. Each module's `init` creates a
/// shared Faucet<T>; we parse them out by module name and pair them with the
/// fixed `TEST_TOKEN_TABLE`. Re-publishing produces a fresh package each
/// time — callers are responsible for overwriting the JSON entry.
pub async fn publish_test_tokens(
    client: &SuiClient,
    signer: &Signer,
    tokens_path: &Path,
    gas_budget: u64,
) -> Result<TestTokensOutcome> {
    tracing::info!(path = %tokens_path.display(), "compiling test-tokens package");
    let compiled = BuildConfig::new_for_testing()
        .build(tokens_path)
        .with_context(|| {
            format!("compiling test-tokens package at {}", tokens_path.display())
        })?;
    let modules = compiled.get_package_bytes(false);
    let deps = compiled.get_dependency_storage_package_ids();

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building test-tokens publish tx")?;
    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting test-tokens publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting test-tokens publish tx")?;
    assert_success(&resp)?;

    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("test-tokens publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;
    // Map module name (e.g. "tusdc") -> Faucet object id.
    let mut faucets: std::collections::HashMap<String, ObjectID> =
        std::collections::HashMap::new();

    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => {
                package_id = Some(*pid);
            }
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
                    continue;
                }
                if object_type.name.as_str() == "Faucet" {
                    faucets.insert(object_type.module.as_str().to_owned(), *object_id);
                }
            }
            _ => {}
        }
    }

    let package_id = package_id
        .ok_or_else(|| anyhow!("test-tokens publish: no Published change"))?;
    let upgrade_cap_id = upgrade_cap_id
        .ok_or_else(|| anyhow!("test-tokens publish: no UpgradeCap created"))?;

    let pkg_hex = package_id.to_hex_uncompressed();
    let mut tokens = Vec::with_capacity(TEST_TOKEN_TABLE.len());
    for (symbol, module, decimals) in TEST_TOKEN_TABLE {
        let faucet_id = faucets
            .remove(*module)
            .ok_or_else(|| anyhow!("test-tokens publish: no Faucet for module `{module}`"))?;
        tokens.push(TestTokenInfo {
            symbol: (*symbol).to_owned(),
            coin_type: format!("{}::{}::{}", pkg_hex, module, symbol),
            faucet_id,
            decimals: *decimals,
        });
    }

    Ok(TestTokensOutcome {
        package_id,
        upgrade_cap_id,
        digest,
        tokens,
    })
}

fn assert_success(resp: &SuiTransactionBlockResponse) -> Result<()> {
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
