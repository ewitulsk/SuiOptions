use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::network::Network;

/// One network's deployment record. Fields are camelCase to match the JSON
/// shape the TS reference produces, so other services already consuming
/// that format don't need changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkDeployment {
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
}

/// On-disk shape: `{ "mainnet": {...}, "testnet": {...}, "devnet": {...} }`.
/// Stored sorted so diffs stay clean across runs.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Deployments {
    pub networks: BTreeMap<String, NetworkDeployment>,
}

impl Deployments {
    /// Reads the file if it exists; returns an empty store if not.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let store = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(store)
    }

    pub fn upsert(&mut self, network: Network, deployment: NetworkDeployment) {
        self.networks.insert(network.as_str().to_owned(), deployment);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        // Always include every network key, even if unset, so consumers can
        // rely on the shape and humans can see what's missing at a glance.
        let mut full = serde_json::Map::new();
        for net in Network::ALL {
            let key = net.as_str().to_owned();
            match self.networks.get(&key) {
                Some(d) => full.insert(key, serde_json::to_value(d)?),
                None => full.insert(key, serde_json::Value::Null),
            };
        }
        let pretty = serde_json::to_vec_pretty(&serde_json::Value::Object(full))?;
        std::fs::write(path, pretty)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
