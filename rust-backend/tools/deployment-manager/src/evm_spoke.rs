//! `--record-evm-spoke`: fold a forge deploy artifact
//! (`evm-contracts/deployments/<env>.json`, written by
//! `script/DeploySpoke.s.sol`) into the env record's
//! `multichain.spokes.<name>` block, so deployments.json stays the ONE
//! place addresses are written (multichain-vault-plan §9).
//!
//! Read-merge-write: only the named spoke's entry is replaced; sibling
//! spokes and every other block on the record are untouched.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::json_store::{EvmSpokeRecord, EvmTokenRecord, MultichainRecord};

/// The artifact `DeploySpoke.s.sol` writes. Field names match the forge
/// script's `vm.serializeJson` keys verbatim.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpokeArtifact {
    /// Spoke name — the `multichain.spokes` map key (e.g. "robinhood").
    pub name: String,
    pub spoke_id: u64,
    pub protocol_chain_id: u64,
    pub evm_chain_id: u64,
    pub spoke_vault: String,
    #[serde(default)]
    pub relayer_endpoint: Option<String>,
    #[serde(default)]
    pub layerzero_endpoint: Option<String>,
    #[serde(default)]
    pub ccip_endpoint: Option<String>,
    pub usdg: UsdgArtifact,
    pub deploy_block: u64,
    pub deployer: String,
    pub deployed_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsdgArtifact {
    pub address: String,
    pub decimals: u8,
    pub asset_code: u8,
}

/// `0x` + 40 hex chars (any case — forge writes checksummed addresses).
fn validate_evm_address(label: &str, addr: &str) -> Result<()> {
    let hex_part = addr
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("{label}: EVM address `{addr}` missing 0x prefix"))?;
    if hex_part.len() != 40 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{label}: `{addr}` is not a 20-byte 0x-hex EVM address");
    }
    Ok(())
}

impl SpokeArtifact {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading spoke artifact {}", path.display()))?;
        let artifact: SpokeArtifact = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing spoke artifact {}", path.display()))?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("spoke artifact has an empty name");
        }
        validate_evm_address("spokeVault", &self.spoke_vault)?;
        validate_evm_address("deployer", &self.deployer)?;
        validate_evm_address("usdg.address", &self.usdg.address)?;
        for (label, ep) in [
            ("relayerEndpoint", &self.relayer_endpoint),
            ("layerzeroEndpoint", &self.layerzero_endpoint),
            ("ccipEndpoint", &self.ccip_endpoint),
        ] {
            if let Some(addr) = ep {
                validate_evm_address(label, addr)?;
            }
        }
        Ok(())
    }
}

/// Replace (or insert) the artifact's spoke in `multichain.spokes`,
/// leaving sibling spokes and the hub fields alone. Returns the spoke
/// name that was written.
pub fn merge_spoke(multichain: &mut MultichainRecord, artifact: &SpokeArtifact) -> String {
    let record = EvmSpokeRecord {
        spoke_id: artifact.spoke_id,
        protocol_chain_id: artifact.protocol_chain_id,
        evm_chain_id: artifact.evm_chain_id,
        spoke_vault: artifact.spoke_vault.clone(),
        relayer_endpoint: artifact.relayer_endpoint.clone(),
        layerzero_endpoint: artifact.layerzero_endpoint.clone(),
        ccip_endpoint: artifact.ccip_endpoint.clone(),
        usdg: EvmTokenRecord {
            address: artifact.usdg.address.clone(),
            decimals: artifact.usdg.decimals,
            asset_code: artifact.usdg.asset_code,
        },
        deploy_block: artifact.deploy_block,
        deployer: artifact.deployer.clone(),
        deployed_at: artifact.deployed_at.clone(),
    };
    multichain.spokes.insert(artifact.name.clone(), record);
    artifact.name.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn artifact(name: &str, vault_suffix: &str) -> SpokeArtifact {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "spokeId": 3,
            "protocolChainId": 257,
            "evmChainId": 46898,
            "spokeVault": format!("0x00000000000000000000000000000000000000{vault_suffix}"),
            "relayerEndpoint": "0x00000000000000000000000000000000000000bb",
            "usdg": {
                "address": "0x00000000000000000000000000000000000000cc",
                "decimals": 6,
                "assetCode": 1
            },
            "deployBlock": 123456,
            "deployer": "0x00000000000000000000000000000000000000dd",
            "deployedAt": "2026-08-30T00:00:00Z"
        }))
        .unwrap()
    }

    fn multichain() -> MultichainRecord {
        MultichainRecord {
            endpoint_registry_id: "0xe1".into(),
            hub_chain_id: 1,
            spokes: BTreeMap::new(),
        }
    }

    #[test]
    fn fresh_insert() {
        let mut mc = multichain();
        let art = artifact("robinhood", "aa");
        art.validate().unwrap();
        let name = merge_spoke(&mut mc, &art);
        assert_eq!(name, "robinhood");
        let spoke = &mc.spokes["robinhood"];
        assert_eq!(spoke.spoke_id, 3);
        assert_eq!(spoke.evm_chain_id, 46898);
        assert_eq!(spoke.usdg.asset_code, 1);
        // Endpoints the script did not deploy stay absent.
        assert!(spoke.layerzero_endpoint.is_none());
        assert!(spoke.ccip_endpoint.is_none());
        // Hub fields untouched.
        assert_eq!(mc.endpoint_registry_id, "0xe1");
        assert_eq!(mc.hub_chain_id, 1);
    }

    #[test]
    fn overwrite_replaces_only_that_spoke() {
        let mut mc = multichain();
        merge_spoke(&mut mc, &artifact("robinhood", "aa"));
        merge_spoke(&mut mc, &artifact("robinhood", "ee"));
        assert_eq!(mc.spokes.len(), 1);
        assert_eq!(
            mc.spokes["robinhood"].spoke_vault,
            "0x00000000000000000000000000000000000000ee"
        );
    }

    #[test]
    fn preserves_sibling_spokes() {
        let mut mc = multichain();
        merge_spoke(&mut mc, &artifact("robinhood", "aa"));
        merge_spoke(&mut mc, &artifact("hyperliquid", "ee"));
        assert_eq!(mc.spokes.len(), 2);
        assert_eq!(
            mc.spokes["robinhood"].spoke_vault,
            "0x00000000000000000000000000000000000000aa"
        );
        assert_eq!(
            mc.spokes["hyperliquid"].spoke_vault,
            "0x00000000000000000000000000000000000000ee"
        );
    }

    #[test]
    fn rejects_malformed_addresses() {
        // Too short.
        let mut bad = artifact("robinhood", "aa");
        bad.spoke_vault = "0x1234".into();
        assert!(bad.validate().is_err());
        // Missing 0x.
        let mut bad = artifact("robinhood", "aa");
        bad.usdg.address = "00000000000000000000000000000000000000cc".into();
        assert!(bad.validate().is_err());
        // Non-hex.
        let mut bad = artifact("robinhood", "aa");
        bad.relayer_endpoint = Some("0x00000000000000000000000000000000000000zz".into());
        assert!(bad.validate().is_err());
        // Checksummed (mixed-case) is fine — forge writes these.
        let mut ok = artifact("robinhood", "aa");
        ok.deployer = "0xAbCd0000000000000000000000000000000000dD".into();
        assert!(ok.validate().is_ok());
    }

    /// The merged record serializes to the exact camelCase shape the
    /// READER (`crates/deployments`) parses — the two sides must not
    /// drift.
    #[test]
    fn round_trips_through_the_reader_schema() {
        let mut mc = multichain();
        merge_spoke(&mut mc, &artifact("robinhood", "aa"));
        let json = serde_json::to_string(&mc).unwrap();
        let reader: deployments::MultichainInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(reader.hub_chain_id, 1);
        let spoke = reader.spoke("robinhood").unwrap();
        assert_eq!(spoke.spoke_id, 3);
        assert_eq!(spoke.protocol_chain_id, 257);
        assert_eq!(
            spoke.relayer_endpoint.as_deref(),
            Some("0x00000000000000000000000000000000000000bb")
        );
        assert!(spoke.layerzero_endpoint.is_none());
        assert_eq!(spoke.usdg.decimals, 6);
        // Absent optional endpoints are omitted, not null.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["spokes"]["robinhood"].get("layerzeroEndpoint").is_none());
    }
}
