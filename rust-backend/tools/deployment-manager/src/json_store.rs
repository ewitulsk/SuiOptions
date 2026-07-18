use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The deployment environments we always render as keys (even when unset),
/// so the on-disk shape is stable and humans can see what's missing.
const ENVS: [&str; 3] = ["dev", "prod", "staging"];

/// One env's deployment record. Two halves:
///   - `package_info` — everything that comes out of publishing Move
///     packages (protocol object ids, the test-token catalog with
///     faucets, deploy digests). Field names inside are camelCase to
///     match the JSON the TS reference produces.
///   - `token_info` — off-chain token catalog (coin type, decimals,
///     optional Pyth feed id). One entry per supported ticker, on every
///     network. On testnet we replicate addresses from `testTokens`; on
///     mainnet the same block lists real assets while `testTokens` is
///     absent.
///
/// The two container keys (`package_info`, `token_info`) are snake_case
/// by intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDeployment {
    pub package_info: PackageInfo,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub token_info: BTreeMap<String, TokenSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInfo {
    pub package_id: String,
    pub admin_cap_id: String,
    pub protocol_config_id: String,
    pub upgrade_cap_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub treasury_id: Option<String>,
    pub publish_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_digest: Option<String>,
    pub deployer: String,
    pub deployed_at: String, // RFC 3339
    pub network: String,
    /// Set when the test-tokens package was published alongside this
    /// deployment (via `--deploy-tokens`). Overwritten on each rerun.
    /// Testnet-only; absent on mainnet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_tokens: Option<TestTokensRecord>,
    /// DeepBook v3 deployment ids (SO-151). Authored by hand, not by this
    /// tool — kept as opaque JSON and carried forward on redeploys so a
    /// rerun never drops the block. Typed access lives in
    /// `crates/deployments::DeepBookInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepbook: Option<serde_json::Value>,
    /// The other three packages of the contracts tree, published in
    /// dependency order alongside the core package (`packageId` above is
    /// the core / options_core package). Typed access lives in
    /// `crates/deployments::SubPackageInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auction: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfq: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<PackageRecord>,
    /// Curated trading-vault package (SO-283) and its Pyth oracle
    /// adapter, published after the options tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trading_vault: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_pyth: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepbook_adapter: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_adapter: Option<PackageRecord>,
    /// Shared governance objects + activation digest for the
    /// trading-vault family (SO-292): written by the post-publish
    /// activation step so services read ids from token-info instead of
    /// re-deriving them from publish digests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trading_vault_objects: Option<TradingVaultObjectsRecord>,
    /// cctp_bridge package (via `--deploy-cctp`); carried forward on
    /// protocol-only redeploys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cctp_bridge: Option<CctpBridgeRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingVaultObjectsRecord {
    pub vault_protocol_config_id: String,
    pub integration_registry_id: String,
    pub oracle_registry_id: String,
    pub pyth_feed_registry_id: String,
    pub pool_allowlist_id: String,
    pub activation_digest: String,
}

/// The published cctp_bridge package (cctp-contracts/).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CctpBridgeRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    pub network: String,
}

/// One published sub-package of the contracts tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
}

/// The published test-tokens package + the shared faucets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestTokensRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    /// Keyed by token symbol (e.g. "TUSDC"). Sorted for clean diffs.
    pub tokens: BTreeMap<String, TestTokenRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestTokenRecord {
    /// Full move type tag, e.g. "0x<pkg>::tusdc::TUSDC".
    pub coin_type: String,
    /// Shared Faucet<T> object ID.
    pub faucet_id: String,
    pub decimals: u8,
}

/// One entry of the off-chain `token_info` catalog. Carries everything
/// off-chain pricers (mm-bot) need to source a USD spot for this ticker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSpec {
    pub coin_type: String,
    pub decimals: u8,
    /// Optional so tokens without a real-world Pyth feed (synthetic
    /// test tokens) still appear in the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pyth_feed_id: Option<String>,
}

/// On-disk shape: `{ "dev": {...}, "prod": {...}, "staging": {...} }`,
/// keyed by deployment environment. The Sui network each record lives on
/// is carried inside it (`package_info.network`). Stored sorted so diffs
/// stay clean across runs.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Deployments {
    pub envs: BTreeMap<String, NetworkDeployment>,
}

impl Deployments {
    /// Reads the file if it exists; returns an empty store if not. Tolerates
    /// `null` entries for un-deployed networks (the shape we ourselves write
    /// on save), so a round-trip from an empty file works.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;

        let mut envs = BTreeMap::new();
        for (key, value) in raw {
            if value.is_null() {
                continue;
            }
            let record: NetworkDeployment = serde_json::from_value(value)
                .with_context(|| format!("parsing {} entry in {}", key, path.display()))?;
            envs.insert(key, record);
        }
        Ok(Self { envs })
    }

    pub fn upsert(&mut self, env: &str, deployment: NetworkDeployment) {
        self.envs.insert(env.to_owned(), deployment);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        // Always include every env key, even if unset, so consumers can
        // rely on the shape and humans can see what's missing at a glance.
        // Any extra keys already in the map are preserved too.
        let mut full = serde_json::Map::new();
        for env in ENVS {
            full.insert(env.to_owned(), serde_json::Value::Null);
        }
        for (env, dep) in &self.envs {
            full.insert(env.clone(), serde_json::to_value(dep)?);
        }
        let pretty = serde_json::to_vec_pretty(&serde_json::Value::Object(full))?;
        std::fs::write(path, pretty)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
