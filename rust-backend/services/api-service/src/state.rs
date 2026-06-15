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
    /// derived-metric-worker read-API base URL (predicted-APY series). `None`
    /// on envs without the worker — the apy endpoint serves realized only.
    pub derived_metrics_url: Option<String>,
    /// Sui fullnode JSON-RPC URL for the live-vault `sui_getObject` read.
    pub sui_rpc_url: String,
    /// price-charting read-API base URL (e.g. `http://price-charting:9013`).
    /// Used to mark exercises at the option-pool price at exercise time in the
    /// FIFO PnL ledger (SO-209). `None` → exercises are left unpriced.
    pub price_charting_url: Option<String>,
    /// Shared HTTP client for composing the worker's read API + the RPC read.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(
        catalog: TokenCatalog,
        indexer_graphql_url: String,
        derived_metrics_url: Option<String>,
        sui_rpc_url: String,
        price_charting_url: Option<String>,
    ) -> Self {
        Self {
            catalog,
            indexer: IndexerClient::new(indexer_graphql_url),
            derived_metrics_url: derived_metrics_url.map(|u| u.trim_end_matches('/').to_string()),
            sui_rpc_url,
            price_charting_url: price_charting_url.map(|u| u.trim_end_matches('/').to_string()),
            http: reqwest::Client::new(),
        }
    }
}
