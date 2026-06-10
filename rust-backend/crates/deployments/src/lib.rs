//! Loader for `deployments.json` — the single source of truth for every
//! on-chain id our off-chain stack needs, plus the off-chain token catalog
//! used for pricing.
//!
//! Shape (matches `deployment-manager`'s output): keyed by deployment
//! environment (`staging` / `prod`), one record each. The Sui
//! network a record lives on is carried inside it as
//! `package_info.network` — that's what lets two envs (e.g. staging and
//! prod) both run on testnet as *distinct* deployments.
//!
//! ```json
//! {
//!   "prod": null,
//!   "staging": {
//!     "package_info": {
//!       "packageId":        "0x…",
//!       "adminCapId":       "0x…",
//!       "protocolConfigId": "0x…",
//!       "treasuryId":       "0x…",
//!       "deployer":         "0x…",
//!       "testTokens": {
//!         "packageId": "0x…",
//!         "tokens": {
//!           "TBTC":  { "coinType": "0x…::tbtc::TBTC", "faucetId": "0x…", "decimals": 8 },
//!           …
//!         }
//!       }
//!     },
//!     "token_info": {
//!       "TBTC":  { "coinType": "0x…::tbtc::TBTC", "decimals": 8, "pythFeedId": "0x…" },
//!       …
//!     }
//!   }
//! }
//! ```
//!
//! `package_info` collects everything that comes from publishing the Move
//! package(s): protocol object ids, the test-token catalog with faucets,
//! deploy digests. `token_info` is the off-chain catalog every consumer
//! reads from for *pricing* — coin types, decimals, and an optional Pyth
//! feed id. On testnet the addresses are replicated from the testTokens
//! deploy outcome; on mainnet the same `token_info` block will list real
//! assets, while `package_info.testTokens` is absent.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sui_types::base_types::{ObjectID, SuiAddress};
use tracing::{debug, info};

/// One deployed test token: its `Coin<T>` Move type, the shared `Faucet`
/// object id that wraps the TreasuryCap, and the coin's decimals.
/// Test-token records are testnet-only; mainnet has no faucets.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// External **DeepBook v3** deployment the record's network trades on.
/// Not an artifact of our publish — a reference to Mysten's deployment —
/// but carried inside `package_info` so token-info serves it to every
/// consumer (frontend PTBs, gas-station templates, indexer event match).
/// Absent where DeepBook isn't deployed (devnet).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepBookInfo {
    /// Current (upgraded) package id — Move calls target this.
    pub package_id: String,
    /// Original publish id. Event/struct type strings resolve to this one
    /// (Sui upgrade semantics), so event matching must use it.
    pub original_package_id: String,
    /// Shared `Registry` object id (`pool::create_permissionless_pool` arg).
    pub registry_id: String,
    /// Fully-qualified DEEP coin type — the pool-creation fee currency.
    pub deep_coin_type: String,
    /// `create_permissionless_pool` fee in DEEP atomic units (6 decimals),
    /// as a string to sidestep JSON number precision.
    pub pool_creation_fee: String,
}

impl DeepBookInfo {
    pub fn package(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_id).context("parsing deepbook packageId")
    }
    pub fn original_package(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.original_package_id)
            .context("parsing deepbook originalPackageId")
    }
    pub fn registry(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.registry_id).context("parsing deepbook registryId")
    }
    /// Pool-creation fee in DEEP atomic units.
    pub fn pool_creation_fee_units(&self) -> Result<u64> {
        self.pool_creation_fee
            .parse::<u64>()
            .context("parsing deepbook poolCreationFee")
    }
}

/// On-chain artifacts from publishing the protocol Move package (and,
/// on testnet, the test-tokens package).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInfo {
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
    /// Test-token package + faucets. Testnet-only; absent on mainnet.
    #[serde(default)]
    pub test_tokens: Option<TestTokens>,
    /// DeepBook v3 ids for this network. Absent where DeepBook isn't
    /// deployed (devnet) — consumers must treat it as optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepbook: Option<DeepBookInfo>,
}

/// Off-chain token catalog entry. One per supported ticker, replicated
/// on testnet from the testTokens deploy and authored by hand on
/// mainnet. Carries everything off-chain pricers (mm-bot) need to source
/// a USD spot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSpec {
    /// Fully-qualified Move type, e.g. `0xpkg::tbtc::TBTC`. On testnet,
    /// must match the same symbol's entry under `package_info.testTokens.tokens`.
    pub coin_type: String,
    pub decimals: u8,
    /// 32-byte hex (with or without `0x` prefix). Optional so tokens
    /// without a real-world Pyth feed (synthetic test tokens) still
    /// appear in the catalog; consumers that need it fail at startup
    /// with a clear message if it's missing for a configured symbol.
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
}

impl TokenSpec {
    /// Typed Pyth feed id. Errors if `pyth_feed_id` is absent or malformed.
    pub fn pyth_feed(&self) -> Result<protocol_types::PriceFeedId> {
        let raw = self
            .pyth_feed_id
            .as_deref()
            .ok_or_else(|| anyhow!("token has no pythFeedId in deployments.json"))?;
        protocol_types::PriceFeedId::from_hex(raw)
    }
}

/// Per-network deployment record. `package_info` carries everything
/// derived from publishing Move packages; `token_info` is the off-chain
/// pricing catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDeployment {
    pub package_info: PackageInfo,
    #[serde(default)]
    pub token_info: BTreeMap<String, TokenSpec>,
}

impl NetworkDeployment {
    pub fn package(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_info.package_id).context("parsing package_id")
    }
    pub fn admin_cap(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_info.admin_cap_id).context("parsing admin_cap_id")
    }
    pub fn protocol_config(&self) -> Result<ObjectID> {
        ObjectID::from_str(&self.package_info.protocol_config_id)
            .context("parsing protocol_config_id")
    }
    pub fn treasury(&self) -> Result<ObjectID> {
        let id = self
            .package_info
            .treasury_id
            .as_deref()
            .ok_or_else(|| anyhow!("treasury_id missing from deployments"))?;
        ObjectID::from_str(id).context("parsing treasury_id")
    }
    pub fn deployer_address(&self) -> Result<SuiAddress> {
        SuiAddress::from_str(&self.package_info.deployer).context("parsing deployer")
    }

    /// Raw bytes of the AdminCap id — the domain separator the chain
    /// compares every `Quote.protocol_id` against (`admin.move:20`).
    pub fn protocol_id_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.admin_cap()?.into_bytes().to_vec())
    }

    /// Test-token catalog (on-chain bootstrap data: faucets + coin types).
    /// Testnet-only. Errors on mainnet.
    pub fn token(&self, symbol: &str) -> Result<&TokenInfo> {
        self.package_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section in deployments.json"))?
            .get(symbol)
    }

    /// `Result`-flavored access to the entire test-tokens block. Errors
    /// if testTokens isn't present.
    pub fn test_tokens(&self) -> Result<&TestTokens> {
        self.package_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section in deployments.json"))
    }

    /// `Option`-flavored access — call sites that already gracefully
    /// handle "no test tokens deployed" (e.g. `info` printout) use this.
    pub fn maybe_test_tokens(&self) -> Option<&TestTokens> {
        self.package_info.test_tokens.as_ref()
    }

    /// Off-chain catalog lookup (coin type, decimals, optional Pyth feed
    /// id). Available on every network — this is what mm-bot's pricing
    /// reads.
    pub fn token_spec(&self, symbol: &str) -> Result<&TokenSpec> {
        let upper = symbol.to_ascii_uppercase();
        self.token_info.get(&upper).ok_or_else(|| {
            anyhow!(
                "no token_info entry for {symbol} (have: {:?})",
                self.token_info.keys().collect::<Vec<_>>()
            )
        })
    }
}

/// All recorded deployments, keyed by environment (`staging` /
/// `prod`). Un-deployed envs are written as `null` (and dropped on load).
#[derive(Debug)]
pub struct Deployments {
    pub envs: BTreeMap<String, NetworkDeployment>,
}

impl Deployments {
    pub fn load(path: &Path) -> Result<Self> {
        info!(path = %path.display(), "loading deployments");
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading deployments file {}", path.display()))?;
        // Tolerate `null` slots for un-deployed envs (the shape
        // deployment-manager writes). Keys are lowercased so lookup is
        // case-insensitive.
        let raw: BTreeMap<String, Option<NetworkDeployment>> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing deployments file {}", path.display()))?;
        let envs: BTreeMap<String, NetworkDeployment> = raw
            .into_iter()
            .filter_map(|(k, v)| v.map(|d| (k.to_ascii_lowercase(), d)))
            .collect();
        debug!(envs = ?envs.keys().collect::<Vec<_>>(), "deployments loaded");
        Ok(Self { envs })
    }

    /// Environment slot lookup. Accepts any casing of `staging` /
    /// `prod`; an env with no recorded deployment errors.
    pub fn for_env(&self, env: &str) -> Result<&NetworkDeployment> {
        self.envs
            .get(&env.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("no deployment recorded for env {env}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_id_is_admin_cap_bytes() {
        let dep = NetworkDeployment {
            package_info: PackageInfo {
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
                deepbook: None,
            },
            token_info: BTreeMap::new(),
        };
        let bytes = dep.protocol_id_bytes().unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x3a);
        assert_eq!(bytes[31], 0xc8);
    }

    #[test]
    fn deepbook_block_roundtrips_and_is_optional() {
        // Absent (devnet / pre-SO-151 files): parses to None and is not
        // re-serialized, so old consumers see an unchanged shape.
        let without: PackageInfo = serde_json::from_str(
            r#"{
                "packageId": "0x1", "adminCapId": "0x2",
                "protocolConfigId": "0x3", "upgradeCapId": "0x4",
                "publishDigest": "d", "deployer": "0x5",
                "deployedAt": "", "network": "devnet"
            }"#,
        )
        .unwrap();
        assert!(without.deepbook.is_none());
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("deepbook"));

        // Present: camelCase fields round-trip and typed accessors parse.
        let db: DeepBookInfo = serde_json::from_str(
            r#"{
                "packageId": "0x22be4cade64bf2d02412c7e8d0e8beea2f78828b948118d46735315409371a3c",
                "originalPackageId": "0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982",
                "registryId": "0x7c256edbda983a2cd6f946655f4bf3f00a41043993781f8674a7046e8c0e11d1",
                "deepCoinType": "0x36dbef866a1d62bf7328989a10fb2f07d769f4ee587c0de4a0a256e57e0a58a8::deep::DEEP",
                "poolCreationFee": "500000000"
            }"#,
        )
        .unwrap();
        assert!(db.package().is_ok());
        assert!(db.original_package().is_ok());
        assert!(db.registry().is_ok());
        assert_eq!(db.pool_creation_fee_units().unwrap(), 500_000_000);
        let back = serde_json::to_string(&db).unwrap();
        assert!(back.contains("originalPackageId"));
        assert!(back.contains("deepCoinType"));
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
        // staging is the populated testnet deployment in the fixture.
        let testnet = d.for_env("staging").unwrap();
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
            // Same set must also be present in token_info.
            let spec = testnet.token_spec(expected).unwrap();
            assert_eq!(spec.coin_type, t.coin_type);
            assert_eq!(spec.decimals, t.decimals);
        }
        // DeepBook ids ride along for testnet-backed envs (SO-151).
        let db = testnet
            .package_info
            .deepbook
            .as_ref()
            .expect("staging deepbook block");
        let _ = db.package().unwrap();
        let _ = db.original_package().unwrap();
        let _ = db.registry().unwrap();
        assert_eq!(db.pool_creation_fee_units().unwrap(), 500_000_000);

        // Pyth feed ids land on the catalog side, not on TokenInfo.
        // All four staging tokens carry feeds since SO-139/SO-141.
        for sym in ["TBTC", "TUSDC", "TWAL", "TDEEP"] {
            assert!(testnet.token_spec(sym).unwrap().pyth_feed().is_ok());
        }
        // Case-insensitive symbol lookup.
        assert!(testnet.token_spec("tbtc").is_ok());

        // Case-insensitive env slot lookup.
        assert!(d.for_env("STAGING").is_ok());
        assert!(d.for_env("prod").is_ok()); // populated since 2026-06-08
        assert!(d.for_env("garbage").is_err());
    }
}
