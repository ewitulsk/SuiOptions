//! Publish contracts + test-tokens against a freshly-spawned localnet.
//!
//! Reuses [`deployment_manager::deploy::publish_package`] /
//! [`deployment_manager::deploy::create_and_share_treasury`] /
//! [`deployment_manager::deploy::publish_test_tokens`] so we exercise
//! exactly the same code path that ships to staging/prod.
//!
//! On return, an ephemeral `deployments.json` has been written and
//! parsed back into [`shared::deployments::Deployments`]. The chain
//! harness writes the file under the test's tempdir so anything
//! downstream (the indexer's `Config::resolve_package_id`, the
//! mm-bot/writer Cli) can point at it directly.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::Utc;
use sui_sdk::SuiClient;
use sui_types::base_types::SuiAddress;

use deployment_manager::deploy::{
    create_and_share_treasury, publish_package, publish_test_tokens,
};
use deployment_manager::json_store::{
    Deployments as DmDeployments, NetworkDeployment, PackageInfo, TestTokenRecord,
    TestTokensRecord, TokenSpec,
};
use deployment_manager::network::Network as DmNetwork;
use deployment_manager::signer::Signer as DmSigner;

use shared::deployments::Deployments;
use shared::sui_client::Signer;

const GAS_BUDGET: u64 = 500_000_000;

/// Result of publishing the protocol + test-tokens against a localnet.
pub struct Deployed {
    /// Path on disk to the written `deployments.json`. Pass this to the
    /// indexer's config + the writer/mm-bot CLIs.
    pub deployments_path: PathBuf,
    /// Parsed in-memory view (same struct the runtime binaries load).
    pub deployments: Deployments,
}

/// Publish the contracts + test-tokens to `localnet`, capturing the
/// resulting object ids into a `deployments.json` written under
/// `out_dir`. The Network slot used is `devnet` — that's the only
/// real `Network` enum variant that isn't tied to a public Sui endpoint
/// in our codebase, and the writer/mm-bot's Cli accepts it.
pub async fn publish_all(
    client: &SuiClient,
    deployer: &Signer,
    out_dir: &Path,
) -> Result<Deployed> {
    let dm_signer = bridge_signer(deployer)?;
    let contracts_path = resolve_repo_path("contracts")?;
    let tokens_path = resolve_repo_path("test-tokens")?;

    tracing::info!(
        path = %contracts_path.display(),
        "publishing protocol contracts"
    );
    let publish = publish_package(client, &dm_signer, &contracts_path, GAS_BUDGET)
        .await
        .context("publishing protocol package")?;
    tracing::info!(
        package_id = %publish.package_id,
        admin_cap = %publish.admin_cap_id,
        protocol_config = %publish.protocol_config_id,
        "protocol published"
    );

    let init = create_and_share_treasury(
        client,
        &dm_signer,
        publish.package_id,
        publish.admin_cap_id,
        GAS_BUDGET,
    )
    .await
    .context("treasury init")?;
    tracing::info!(treasury = %init.treasury_id, "treasury created");

    tracing::info!(path = %tokens_path.display(), "publishing test tokens");
    let tokens = publish_test_tokens(client, &dm_signer, &tokens_path, GAS_BUDGET)
        .await
        .context("publishing test-tokens")?;
    tracing::info!(
        tokens_package = %tokens.package_id,
        token_count = tokens.tokens.len(),
        "test-tokens published"
    );

    // Compose the on-disk shape. `package_info` mirrors what
    // `deployment-manager`'s main.rs would produce after a real
    // `deploy --deploy-tokens` run.
    let now = Utc::now().to_rfc3339();
    let mut token_records = std::collections::BTreeMap::new();
    let mut token_specs = std::collections::BTreeMap::new();
    for t in &tokens.tokens {
        token_records.insert(
            t.symbol.clone(),
            TestTokenRecord {
                coin_type: t.coin_type.clone(),
                faucet_id: t.faucet_id.to_string(),
                decimals: t.decimals,
            },
        );
        token_specs.insert(
            t.symbol.clone(),
            TokenSpec {
                coin_type: t.coin_type.clone(),
                decimals: t.decimals,
                // No real Pyth feed for the localnet — leaving this
                // None means the mm-bot's pricing path can't run
                // against this deployment. Tests that need on-chain
                // mm-bot pricing must construct a feed mock; tests
                // using MockMm do not.
                pyth_feed_id: None,
            },
        );
    }

    let network = DmNetwork::Devnet;
    let record = NetworkDeployment {
        package_info: PackageInfo {
            package_id: publish.package_id.to_string(),
            admin_cap_id: publish.admin_cap_id.to_string(),
            protocol_config_id: publish.protocol_config_id.to_string(),
            upgrade_cap_id: publish.upgrade_cap_id.to_string(),
            treasury_id: Some(init.treasury_id.to_string()),
            publish_digest: publish.digest,
            init_digest: Some(init.digest),
            deployer: deployer.address.to_string(),
            deployed_at: now.clone(),
            network: network.as_str().into(),
            test_tokens: Some(TestTokensRecord {
                package_id: tokens.package_id.to_string(),
                upgrade_cap_id: tokens.upgrade_cap_id.to_string(),
                publish_digest: tokens.digest,
                deployed_at: now,
                tokens: token_records,
            }),
        },
        token_info: token_specs,
    };

    let mut store = DmDeployments::default();
    store.upsert(network, record);

    let deployments_path = out_dir.join("deployments.json");
    store
        .save(&deployments_path)
        .with_context(|| format!("writing {}", deployments_path.display()))?;
    tracing::info!(path = %deployments_path.display(), "wrote ephemeral deployments.json");

    let deployments = Deployments::load(&deployments_path)
        .with_context(|| format!("re-reading {}", deployments_path.display()))?;

    Ok(Deployed {
        deployments_path,
        deployments,
    })
}

/// Convert our `shared::sui_client::Signer` into the deployment-manager's
/// signer type via a bech32 roundtrip — both decoders accept the same
/// `suiprivkey1…` encoding that `SuiKeyPair::encode` produces.
fn bridge_signer(s: &Signer) -> Result<DmSigner> {
    let encoded = s
        .keypair
        .encode()
        .map_err(|e| anyhow::anyhow!("encoding deployer keypair as suiprivkey: {e}"))?;
    let dm = DmSigner::from_string(&encoded)
        .context("re-decoding deployer keypair for deployment-manager")?;
    debug_assert_eq!(dm.address, s.address);
    Ok(dm)
}

/// Resolve `<workspace_root>/<sub>` from the tests-crate manifest dir.
/// `rust-backend/tests` → `..` → `rust-backend` → `..` → workspace root.
fn resolve_repo_path(sub: &str) -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join(sub))
        .ok_or_else(|| anyhow::anyhow!("couldn't resolve repo root for {sub}"))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    Ok(canonical)
}

/// Convenience: parse a `0x…` ObjectID string out of a deployments
/// record into a `SuiAddress` (mostly used by tests that need the
/// deployer address back out as a typed value).
pub fn parse_address(s: &str) -> Result<SuiAddress> {
    SuiAddress::from_str(s).context("parsing address")
}
