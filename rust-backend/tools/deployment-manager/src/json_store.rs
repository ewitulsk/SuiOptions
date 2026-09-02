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
    /// Shared `BucketRegistry` (any-strike derived bucket UIDs, SO-393).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_registry_id: Option<String>,
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
    /// Switchboard oracle adapter (SO-335).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_switchboard: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepbook_adapter: Option<PackageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_adapter: Option<PackageRecord>,
    /// Hybrid-exchange maker adapter for the trading vault (SO-370).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_adapter: Option<PackageRecord>,
    /// Keeper-posted EquityBook package backing external-account equity
    /// (SO-299).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equity_oracle: Option<PackageRecord>,
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
    /// Standalone ingress whitelist package (guarded launch): published
    /// FIRST — every gated package (core, trading-vault, exchange,
    /// exchange-adapter) links against it — with its shared `Whitelist`
    /// object and owned `AdminCap`. The one whitelist, the one admin
    /// surface, for the whole protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whitelist: Option<WhitelistRecord>,
    /// Hybrid-exchange settlement package (via `--deploy-exchange`);
    /// carried forward on protocol-only redeploys. Never republished
    /// implicitly: market registry object IDs are the order-signature
    /// domain and stay bound to the package that created them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange: Option<ExchangeRecord>,
    /// Permissionless option-market listing leaf (SO-416); republishes
    /// with the exchange it links against — its ListingCap is minted by
    /// the exchange's init and parked in the ceremony.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_listing: Option<ExchangeListingRecord>,
    /// mm-bot's shared `QuoteSigner` object for this deployment, created
    /// by `--deploy-mm-collateral` right after the core republish. Reset
    /// to None on every fresh protocol publish — the object's Move type
    /// is package-bound, so a signer never survives a republish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_signer_id: Option<String>,
    /// LayerZero transport package (contracts/endpoint-layerzero, via
    /// `--deploy-endpoints`); carried forward on protocol-only redeploys
    /// like `cctp_bridge`. Typed access: `deployments::EndpointLzInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_layerzero: Option<EndpointLzRecord>,
    /// CCIP transport package (contracts/endpoint-ccip, via
    /// `--deploy-endpoints`); same carry-forward discipline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_ccip: Option<EndpointCcipRecord>,
    /// Multichain hub wiring + EVM spoke deployments (multichain-vault-plan
    /// §9). `endpointRegistryId`/`hubChainId` refresh on every protocol
    /// publish (the registry is created by trading-vault-v2's `endpoint`
    /// module init); `spokes` is written by `--record-evm-spoke` and
    /// preserved across rewrites. Typed access: `deployments::MultichainInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multichain: Option<MultichainRecord>,
}

/// `endpoint_lz` package plus the shared objects its `init` creates: the
/// `LzTransport` and the LayerZero `OApp` object it registers
/// (`LzTransport.oapp_address`, created in the same publish tx).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointLzRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    /// Shared `endpoint_lz::LzTransport` object id.
    pub transport_id: String,
    /// The OApp object address (`LzTransport.oapp_address`).
    pub oapp_id: String,
}

/// `endpoint_ccip` package plus its shared `CcipTransport` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointCcipRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    /// Shared `endpoint_ccip::CcipTransport` object id.
    pub transport_id: String,
}

/// Multichain vault wiring (writer side of `deployments::MultichainInfo`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultichainRecord {
    /// Shared `vault_v2::endpoint::EndpointRegistry` object id.
    pub endpoint_registry_id: String,
    /// Protocol chain id of the hub (envelope namespace, plan §2.1).
    pub hub_chain_id: u64,
    /// Deployed EVM spokes keyed by spoke name (e.g. "robinhood").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub spokes: BTreeMap<String, EvmSpokeRecord>,
}

/// One deployed EVM spoke, merged from the forge deploy artifact by
/// `--record-evm-spoke` (writer side of `deployments::EvmSpokeInfo`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmSpokeRecord {
    /// Hub-side spoke id (`bind_spoke` key).
    pub spoke_id: u64,
    /// Protocol chain id (envelope namespace).
    pub protocol_chain_id: u64,
    /// The EVM network's chain id (eth_chainId).
    pub evm_chain_id: u64,
    /// `SpokeVault` contract address.
    pub spoke_vault: String,
    /// Endpoint contracts actually deployed on this spoke; absent ones
    /// were not deployed for this network set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relayer_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layerzero_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ccip_endpoint: Option<String>,
    /// The spoke deposit asset (USDG mainnet / TUSDG testnet).
    pub usdg: EvmTokenRecord,
    /// Block the deployment landed in (log-scan lower bound).
    pub deploy_block: u64,
    pub deployer: String,
    pub deployed_at: String,
}

/// One EVM token as the spoke vault knows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmTokenRecord {
    pub address: String,
    pub decimals: u8,
    /// Spoke-local asset code carried on the wire (plan §2.1).
    pub asset_code: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingVaultObjectsRecord {
    pub vault_protocol_config_id: String,
    pub integration_registry_id: String,
    pub oracle_registry_id: String,
    pub pyth_feed_registry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switchboard_feed_registry_id: Option<String>,
    pub pool_allowlist_id: String,
    /// Shared `EquityBook` created by the equity-oracle publish (SO-299),
    /// so the keeper reads it from token-info instead of publish effects.
    /// Optional only for READING records written before that step — every
    /// fresh deploy writes both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equity_book_id: Option<String>,
    /// Shared `VolBook` created by the options-adapter publish (premium
    /// mark-to-market), same discipline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vol_book_id: Option<String>,
    /// Ed25519 registrar pubkey seeded into the `VaultProtocolConfig`
    /// (SO-308). Absent = the env deployed with attested self-serve
    /// external-account registration disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registrar_pubkey: Option<String>,
    /// v2 terms provenance (SO-418, plan §9.5.6): the normative spec
    /// version this deployment ships under…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_version: Option<u64>,
    /// …and the hex sha256 of the exact spec document
    /// (docs/trading-vault-v2/spec.md), embedded at compile time so the
    /// recorded hash always matches the checkout the deploy ran from.
    /// Optional only for READING pre-v2 records — every fresh deploy
    /// writes both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_hash: Option<String>,
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

/// The published standalone whitelist package (contracts/whitelist/) plus
/// the two objects its `init` creates: the shared ingress `Whitelist` and
/// the deployer-owned `AdminCap` that gates every mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhitelistRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    /// Shared `Whitelist` object — the gate arg every ingress entry takes.
    pub whitelist_id: String,
    /// Owned `whitelist::AdminCap` (deployer wallet).
    pub admin_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
}

/// The published exchange-listing package (contracts/exchange-listing/,
/// SO-416): the leaf that parks the exchange `ListingCap` in its shared
/// `ListingAuthority` so option markets list permissionlessly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeListingRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    /// Shared `ListingAuthority` (parked ListingCap + dedup + defaults).
    pub listing_authority_id: String,
    /// Owned `exchange_listing::AdminCap` (deployer wallet).
    pub admin_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
}

/// The published hybrid-exchange settlement package (contracts/exchange/)
/// plus its market registries. `markets` maps a human symbol (e.g.
/// "SUI/USDC") to the shared SettlementRegistry object id — the id every
/// order signature is domain-bound to, and the id services must read from
/// here rather than from hand-maintained config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeRecord {
    pub package_id: String,
    pub upgrade_cap_id: String,
    pub admin_cap_id: String,
    pub publish_digest: String,
    pub deployed_at: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub markets: BTreeMap<String, ExchangeMarketRecord>,
}

/// One created exchange market: the registry id plus the config it was
/// created with (mirrored off-chain by the orderbook service).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeMarketRecord {
    pub registry_id: String,
    pub base: String,
    pub quote: String,
    pub tick_size: u64,
    pub min_size: u64,
    pub lot_size: u64,
    pub fee_bps: u64,
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
    /// Switchboard feed hash (SO-335), same shape and same optionality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switchboard_feed_id: Option<String>,
}

impl TokenSpec {
    /// Raw feed key for `provider`, if this token has one. Mirrors
    /// `deployments::TokenSpec::feed_for` — the two structs are separate
    /// because the writer (this tool) and the reader (`deployments`)
    /// evolve independently, but their feed semantics must not diverge.
    pub fn feed_for(&self, provider: protocol_types::OracleProvider) -> Option<&str> {
        match provider {
            protocol_types::OracleProvider::Pyth => self.pyth_feed_id.as_deref(),
            protocol_types::OracleProvider::Switchboard => self.switchboard_feed_id.as_deref(),
        }
    }
}

#[cfg(test)]
mod endpoint_record_tests {
    use super::*;

    /// The writer-side endpoint records serialize to the exact camelCase
    /// shape the READER (`crates/deployments`) parses.
    #[test]
    fn endpoint_records_round_trip_through_the_reader_schema() {
        let lz = EndpointLzRecord {
            package_id: "0x1".into(),
            upgrade_cap_id: "0x2".into(),
            publish_digest: "d".into(),
            deployed_at: "2026-08-30T00:00:00Z".into(),
            transport_id: "0x3".into(),
            oapp_id: "0x4".into(),
        };
        let reader: deployments::EndpointLzInfo =
            serde_json::from_str(&serde_json::to_string(&lz).unwrap()).unwrap();
        assert_eq!(reader.transport_id, "0x3");
        assert_eq!(reader.oapp_id, "0x4");

        let ccip = EndpointCcipRecord {
            package_id: "0x1".into(),
            upgrade_cap_id: "0x2".into(),
            publish_digest: "d".into(),
            deployed_at: "2026-08-30T00:00:00Z".into(),
            transport_id: "0x5".into(),
        };
        let reader: deployments::EndpointCcipInfo =
            serde_json::from_str(&serde_json::to_string(&ccip).unwrap()).unwrap();
        assert_eq!(reader.transport_id, "0x5");
    }
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
