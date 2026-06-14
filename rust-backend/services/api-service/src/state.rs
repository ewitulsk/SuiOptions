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
    /// Shared HTTP client for composing the worker's read API.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(
        catalog: TokenCatalog,
        indexer_graphql_url: String,
        derived_metrics_url: Option<String>,
    ) -> Self {
        Self {
            catalog,
            indexer: IndexerClient::new(indexer_graphql_url),
            derived_metrics_url: derived_metrics_url.map(|u| u.trim_end_matches('/').to_string()),
            http: reqwest::Client::new(),
        }
    }
}
