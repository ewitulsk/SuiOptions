use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use oracle_client::WsMessage;
use pyth_client::{BenchmarkVol, PriceCache, PriceFeedId};
use tokio::sync::broadcast;

/// Shared handler state. Cheap to `Arc`-clone; every field is internally shared.
pub struct AppState {
    /// Live prices, written by the fanout drain loop from the one SSE stream.
    pub price_cache: PriceCache,
    /// Cached + paced realized-vol client, shared across every `/vol/realized`
    /// request so its `(feed, day)` cache and request pacer span all callers.
    pub benchmark_vol: Arc<BenchmarkVol>,
    /// Fanout hub: the drain loop publishes price/status frames; each `/ws`
    /// connection subscribes a receiver.
    pub fanout: broadcast::Sender<WsMessage>,
    /// Feeds the SSE data plane subscribes to (what `/prices` enumerates).
    ///
    /// ALWAYS the Pyth set, regardless of the live provider (SO-346):
    /// spot/vol streaming is off-chain market data with exactly one live
    /// streaming source (Hermes), and every WS consumer (mm-bot's hot
    /// path) keys its cache by these ids. The provider switch governs
    /// ON-CHAIN attestation — the descriptor and `/oracle/legs` — not
    /// where the ticker feed comes from.
    pub feeds: Vec<PriceFeedId>,
    /// The live oracle provider (SO-335) — served on
    /// `/oracle/descriptor` so PTB composers build the right price legs.
    pub provider: protocol_types::OracleProvider,
    /// Canonical coin type → PYTH feed, for the data plane
    /// (`/prices/by-asset`). See `feeds` for why this stays Pyth-keyed.
    pub feed_by_asset: std::collections::BTreeMap<String, PriceFeedId>,
    /// Canonical coin type → the LIVE provider's feed key, for the
    /// descriptor and `/oracle/legs`. Identical to `feed_by_asset` when
    /// the provider is Pyth.
    pub descriptor_feeds: std::collections::BTreeMap<String, PriceFeedId>,
    /// Adapter package + feed registry the composers need, mirrored from
    /// token-info. `None` until the live provider's adapter is deployed.
    pub adapter: Option<crate::state::AdapterIds>,
    /// Off-chain payload source for `GET /oracle/legs` (SO-346).
    pub legs: LegsBackend,
    /// Whether the upstream Hermes stream is currently healthy.
    pub upstream_healthy: Arc<AtomicBool>,
}

/// The live provider's off-chain payload source, backing
/// `GET /oracle/legs` (SO-346). Fixed at boot alongside `provider` — a
/// switch is an oracle-service restart, same as the descriptor.
pub enum LegsBackend {
    /// Hermes accumulator updates, fetched with the same authenticated
    /// client as the SSE data plane.
    Pyth {
        http: reqwest::Client,
        hermes_url: String,
    },
    /// Signed Crossbar consensus payloads. The oracle map comes from the
    /// CHAIN (the queue's `existing_oracles` table, SO-346) — resolved at
    /// boot and re-resolved on a quote failure, because oracle validity
    /// is 7 days and a signer registered after boot would otherwise
    /// wedge legs until a restart.
    Switchboard {
        crossbar: switchboard_client::CrossbarClient,
        /// `oracle_key` (lowercase hex) → object id + attested secp key.
        oracles: tokio::sync::RwLock<std::collections::BTreeMap<String, switchboard_client::OracleInfo>>,
        /// Sui JSON-RPC the map is (re-)resolved against.
        sui_rpc_url: String,
        queue_id: sui_types::base_types::ObjectID,
        queue_key: String,
        /// Switchboard's `on_demand` package id, from config.
        switchboard_package_id: String,
    },
}

/// On-chain identity of the live provider's adapter, as served on
/// `/oracle/descriptor`. Mirrored from token-info at boot so composers
/// have exactly one place to read it from.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct AdapterIds {
    /// Our adapter package (`oracle_pyth` / `oracle_switchboard`).
    pub adapter_package_id: sui_types::base_types::ObjectID,
    /// That adapter's shared feed registry.
    pub feed_registry_id: sui_types::base_types::ObjectID,
    /// `trading_vault::registry::OracleRegistry`.
    pub oracle_registry_id: sui_types::base_types::ObjectID,
}
