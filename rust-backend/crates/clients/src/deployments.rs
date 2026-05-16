//! Loader for `deployments.json`.
//!
//! Shape (mirrors the `deployment-manager` output):
//!
//! ```json
//! {
//!   "mainnet": null,
//!   "testnet": { "packageId": "0x…", "adminCapId": "0x…", … },
//!   "devnet":  null
//! }
//! ```
//!
//! The on-chain `protocol_id` carried in every `Quote` is `bcs(adminCapId)` —
//! 32 raw bytes of the AdminCap ID, derived at deploy in `admin.move:20`. We
//! re-derive it here so clients don't have to know that detail.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;
use sui_types::base_types::{ObjectID, SuiAddress};

/// Per-network deployment record. Field names match the JSON `camelCase`
/// produced by `deployment-manager`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkDeployment {
    pub package_id: String,
    pub admin_cap_id: String,
    pub protocol_config_id: String,
    pub upgrade_cap_id: String,
    pub treasury_id: Option<String>,
    pub publish_digest: String,
    pub init_digest: Option<String>,
    pub deployer: String,
    pub deployed_at: String,
    pub network: String,
}

impl NetworkDeployment {
    pub fn package(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_id).context("parsing package_id")
    }
    pub fn admin_cap(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.admin_cap_id).context("parsing admin_cap_id")
    }
    pub fn protocol_config(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.protocol_config_id).context("parsing protocol_config_id")
    }
    pub fn treasury(&self) -> Result<ObjectID> {
        let id = self
            .treasury_id
            .as_deref()
            .ok_or_else(|| anyhow!("treasury_id missing from deployments"))?;
        ObjectID::from_str(id).context("parsing treasury_id")
    }
    pub fn deployer_address(&self) -> Result<SuiAddress> {
        SuiAddress::from_str(&self.deployer).context("parsing deployer")
    }

    /// Raw bytes of the AdminCap id — the domain separator the chain compares
    /// every `Quote.protocol_id` against (`admin.move:20`).
    pub fn protocol_id_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.admin_cap()?.into_bytes().to_vec())
    }
}

#[derive(Debug, Deserialize)]
pub struct Deployments {
    pub mainnet: Option<NetworkDeployment>,
    pub testnet: Option<NetworkDeployment>,
    pub devnet: Option<NetworkDeployment>,
}

impl Deployments {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading deployments file {}", path.display()))?;
        let parsed: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing deployments file {}", path.display()))?;
        Ok(parsed)
    }

    pub fn for_network(&self, network: crate::sui_client::Network) -> Result<&NetworkDeployment> {
        let slot = match network {
            crate::sui_client::Network::Mainnet => &self.mainnet,
            crate::sui_client::Network::Testnet => &self.testnet,
            crate::sui_client::Network::Devnet => &self.devnet,
        };
        slot.as_ref()
            .ok_or_else(|| anyhow!("no deployment recorded for {network}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock in the protocol_id derivation: it must equal the raw bytes of the
    /// AdminCap ID. If we ever miscompute this, every signed quote breaks.
    #[test]
    fn protocol_id_is_admin_cap_bytes() {
        let dep = NetworkDeployment {
            package_id: "0x59fa88e2a647975902dce0274fcacf2b36ff2aa771d43ac2238826a26404fb66"
                .into(),
            admin_cap_id: "0x99d7e9578236f49aee74b1d673a0e349bca8b92c680e0b9cff9ae27bca331925"
                .into(),
            protocol_config_id: "0x0".into(),
            upgrade_cap_id: "0x0".into(),
            treasury_id: None,
            publish_digest: "x".into(),
            init_digest: None,
            deployer: "0x0".into(),
            deployed_at: "".into(),
            network: "testnet".into(),
        };
        let bytes = dep.protocol_id_bytes().unwrap();
        assert_eq!(bytes.len(), 32);
        // First byte of the admin cap id.
        assert_eq!(bytes[0], 0x99);
        assert_eq!(bytes[31], 0x25);
    }

    #[test]
    fn loads_the_repo_deployments_file() {
        // The committed file at the workspace root has a testnet entry; this
        // test catches accidental schema drift (renaming a JSON field would
        // break every client at runtime).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deployments.json");
        if !path.exists() {
            eprintln!("deployments.json absent at {}; skipping", path.display());
            return;
        }
        let d = Deployments::load(&path).unwrap();
        let testnet = d.testnet.expect("testnet section");
        let _ = testnet.package().unwrap();
        let _ = testnet.admin_cap().unwrap();
        let _ = testnet.protocol_config().unwrap();
        let _ = testnet.treasury().unwrap();
        assert_eq!(testnet.protocol_id_bytes().unwrap().len(), 32);
    }
}
