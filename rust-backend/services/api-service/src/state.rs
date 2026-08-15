//! Shared application state.
//!
//! api-service holds no protocol state of its own: every read is a
//! just-in-time GraphQL query to the indexer (see [`indexer_graphql`]). This
//! struct is just the token catalog plus the indexer client.

use crate::catalog::TokenCatalog;

pub use indexer_graphql::{Account, Bucket as IndexerBucket, IndexerClient, Position, Progress};

pub struct AppState {
    pub catalog: TokenCatalog,
    /// JIT client for the indexer's GraphQL + progress API.
    pub indexer: IndexerClient,
    /// Predicted-APY read-API base URL (now price-charting's `/vault-apy/:id`,
    /// after the derived-metric-worker was folded into it). `None` on envs that
    /// don't run it — the apy endpoint then serves realized only.
    pub derived_metrics_url: Option<String>,
    /// Sui GraphQL RPC URL for the live-vault `object` query.
    pub sui_graphql_url: String,
    /// price-charting read-API base URL (e.g. `http://price-charting:9013`).
    /// Used to mark exercises at the option-pool price at exercise time in the
    /// FIFO PnL ledger (SO-209). `None` → exercises are left unpriced.
    pub price_charting_url: Option<String>,
    /// exchange_adapter package id from the token-info snapshot (SO-372).
    /// Names the direct-quoting adapter witness on trading-vault views;
    /// `None` on deployments without the adapter.
    pub exchange_adapter_package: Option<String>,
    /// options_core package id (SO-394): names the `option_coin` types the
    /// `/buckets/spec` endpoint derives. `None` before the any-strike
    /// redeploy — the endpoint then omits `option_coin_type`.
    pub options_package: Option<String>,
    /// Data-room gold reader for /analytics/* (SO-389). `None` when
    /// `data_room_url` is unset or failed to parse — endpoints then 503.
    pub analytics: Option<std::sync::Arc<crate::analytics::lake::Lake>>,
    /// oracle-service reader for the `/buckets` strike ladder (SO-400): spot
    /// anchors the lattice, realized vol sizes its window. `None` when
    /// `oracle_url` is unset — `/buckets` then serves existing buckets only.
    pub oracle: Option<oracle_client::OracleClient>,
    /// Series families the ladder lists. Empty ⇒ no synthetic strikes.
    pub ladder_pairs: Vec<crate::ladder::LadderPair>,
    /// Shared HTTP client for composing the worker's read API + the RPC read.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(
        catalog: TokenCatalog,
        indexer_graphql_url: String,
        derived_metrics_url: Option<String>,
        sui_graphql_url: String,
        price_charting_url: Option<String>,
        exchange_adapter_package: Option<String>,
        options_package: Option<String>,
        analytics: Option<std::sync::Arc<crate::analytics::lake::Lake>>,
    ) -> Self {
        Self {
            catalog,
            indexer: IndexerClient::new(indexer_graphql_url),
            derived_metrics_url: derived_metrics_url.map(|u| u.trim_end_matches('/').to_string()),
            sui_graphql_url,
            price_charting_url: price_charting_url.map(|u| u.trim_end_matches('/').to_string()),
            exchange_adapter_package,
            options_package,
            analytics,
            oracle: None,
            ladder_pairs: Vec::new(),
            http: reqwest::Client::new(),
        }
    }

    /// Attach the `/buckets` strike ladder (SO-400). Kept off the constructor
    /// so the feature stays opt-in end to end: no `oracle_url`, or no
    /// configured pairs, and `/buckets` serves exactly what the indexer has.
    pub fn with_ladder(
        mut self,
        oracle_url: Option<String>,
        ladder_pairs: Vec<crate::ladder::LadderPair>,
    ) -> Self {
        self.oracle = oracle_url.map(|u| oracle_client::OracleClient::new(u.trim_end_matches('/')));
        self.ladder_pairs = ladder_pairs;
        self
    }
}
