//! Loader for `deployments.json` — the single source of truth for every
//! on-chain id our off-chain stack needs.
//!
//! Shape (matches `deployment-manager`'s output):
//!
//! ```json
//! {
//!   "mainnet": null,
//!   "testnet": {
//!     "packageId":        "0x…",
//!     "adminCapId":       "0x…",
//!     "protocolConfigId": "0x…",
//!     "treasuryId":       "0x…",
//!     "deployer":         "0x…",
//!     "testTokens": {
//!       "packageId": "0x…",
//!       "tokens": {
//!         "TBTC":  { "coinType": "0x…::tbtc::TBTC",  "faucetId": "0x…", "decimals": 8 },
//!         …
//!       }
//!     }
//!   },
//!   "devnet": null
//! }
//! ```
//!
//! Consumers: `clients` (every CLI/bot), `indexer` (resolves `package_id`
//! for event-type strings). Keeping this in its own tiny crate avoids
//! pulling the `clients` workspace of Sui SDK + transaction-builder deps
//! into the indexer.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sui_types::base_types::{ObjectID, SuiAddress};

/// One deployed test token: its `Coin<T>` Move type, the shared `Faucet`
/// object id that wraps the TreasuryCap, and the coin's decimals.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    /// Fully-qualified Move type, e.g. `0xpkg::tbtc::TBTC`.
    pub coin_type: String,
    /// Shared `Faucet` object id (holds `TreasuryCap<T>` per
    /// `test-tokens/sources/{symbol}.move`).
    pub faucet_id: String,
    pub decimals: u8,
}

impl TokenInfo {
    pub fn faucet(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.faucet_id).context("parsing faucet_id")
    }

    /// `(package_id, module_name)` parsed out of `coin_type`. The Move
    /// faucet helpers (`mint`, `mint_to_sender`) live in
    /// `test_tokens::<module>`; that module name is the second `::`
    /// segment (`0xpkg::tbtc::TBTC` → `("0xpkg", "tbtc")`).
    pub fn module_path(&self) -> Result<(ObjectID, String)> {
        let mut parts = self.coin_type.splitn(3, "::");
        let pkg = parts
            .next()
            .ok_or_else(|| anyhow!("malformed coin_type: {}", self.coin_type))?;
        let module = parts
            .next()
            .ok_or_else(|| anyhow!("coin_type missing module: {}", self.coin_type))?;
        let _struct_name = parts
            .next()
            .ok_or_else(|| anyhow!("coin_type missing struct: {}", self.coin_type))?;
        Ok((
            ObjectID::from_str(pkg).context("parsing test token package id")?,
            module.to_owned(),
        ))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestTokens {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    /// Symbol → token info. Symbols are uppercase by convention
    /// (`TBTC`, `TUSDC`, …).
    pub tokens: BTreeMap<String, TokenInfo>,
}

impl TestTokens {
    pub fn package(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_id).context("parsing test tokens package_id")
    }

    /// Case-insensitive lookup. `get("tbtc")` and `get("TBTC")` agree.
    pub fn get(&self, symbol: &str) -> Result<&TokenInfo> {
        let upper = symbol.to_ascii_uppercase();
        self.tokens
            .get(&upper)
            .ok_or_else(|| anyhow!("no test token named {symbol} (have: {:?})", self.symbols()))
    }

    pub fn symbols(&self) -> Vec<&str> {
        self.tokens.keys().map(|s| s.as_str()).collect()
    }
}

/// Per-network deployment record. Field names match the camelCase JSON
/// `deployment-manager` writes.
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
    /// Test-token package + faucets. Optional so older deployment records
    /// without test tokens still load.
    #[serde(default)]
    pub test_tokens: Option<TestTokens>,
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

    /// Raw bytes of the AdminCap id — the domain separator the chain
    /// compares every `Quote.protocol_id` against (`admin.move:20`).
    pub fn protocol_id_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.admin_cap()?.into_bytes().to_vec())
    }

    pub fn token(&self, symbol: &str) -> Result<&TokenInfo> {
        self.test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section in deployments.json"))?
            .get(symbol)
    }

    pub fn test_tokens(&self) -> Result<&TestTokens> {
        self.test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section in deployments.json"))
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

    /// Network slot lookup. Accepts any casing of "mainnet" / "testnet" /
    /// "devnet"; everything else errors.
    pub fn for_network(&self, name: &str) -> Result<&NetworkDeployment> {
        let slot = match name.to_ascii_lowercase().as_str() {
            "mainnet" => &self.mainnet,
            "testnet" => &self.testnet,
            "devnet" => &self.devnet,
            other => return Err(anyhow!("unknown network slot: {other}")),
        };
        slot.as_ref()
            .ok_or_else(|| anyhow!("no deployment recorded for {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_id_is_admin_cap_bytes() {
        let dep = NetworkDeployment {
            package_id: "0x729944d713df56fb9adf6c11acdff215cf77942227834edeb6f44079784aa2aa"
                .into(),
            admin_cap_id: "0x3a094ab9d022f51ef18271e1226c32405df85b4fada60492383de59324b191c8"
                .into(),
            protocol_config_id: "0x0".into(),
            upgrade_cap_id: "0x0".into(),
            treasury_id: None,
            publish_digest: "x".into(),
            init_digest: None,
            deployer: "0x0".into(),
            deployed_at: "".into(),
            network: "testnet".into(),
            test_tokens: None,
        };
        let bytes = dep.protocol_id_bytes().unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x3a);
        assert_eq!(bytes[31], 0xc8);
    }

    #[test]
    fn token_info_parses_package_and_module() {
        let t = TokenInfo {
            coin_type:
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC"
                    .into(),
            faucet_id: "0xaec88534a8aff8868b99995b92593f17d07c396a2c99ae4838ed6b57a8beeef5"
                .into(),
            decimals: 8,
        };
        let (pkg, module) = t.module_path().unwrap();
        assert_eq!(
            pkg.to_string(),
            "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843"
        );
        assert_eq!(module, "tbtc");
    }

    #[test]
    fn loads_the_repo_deployments_file() {
        // `crates/deployments` is two levels below the workspace root.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deployments.json");
        if !path.exists() {
            eprintln!("deployments.json absent at {}; skipping", path.display());
            return;
        }
        let d = Deployments::load(&path).unwrap();
        let testnet = d.for_network("testnet").unwrap();
        let _ = testnet.package().unwrap();
        let _ = testnet.admin_cap().unwrap();
        let _ = testnet.protocol_config().unwrap();
        let _ = testnet.treasury().unwrap();
        assert_eq!(testnet.protocol_id_bytes().unwrap().len(), 32);

        let tokens = testnet.test_tokens().expect("testTokens populated");
        let _ = tokens.package().unwrap();
        for expected in ["TBTC", "TDEEP", "TUSDC", "TWAL"] {
            let t = tokens.get(expected).unwrap();
            let (_pkg, module) = t.module_path().unwrap();
            assert_eq!(module, expected.to_ascii_lowercase());
            let _ = t.faucet().unwrap();
        }
        assert!(tokens.get("tbtc").is_ok());

        // Case-insensitive network slot lookup.
        assert!(d.for_network("TESTNET").is_ok());
        assert!(d.for_network("mainnet").is_err()); // null in fixture
        assert!(d.for_network("garbage").is_err());
    }
}
