//! Shared application state.
//!
//! api-service holds no protocol state of its own: every read is a
//! just-in-time GraphQL query to the indexer (see [`indexer_graphql`]). This
//! struct is just the token catalog plus the indexer client.

use crate::catalog::TokenCatalog;

pub use indexer_graphql::{Account, Bucket as IndexerBucket, IndexerClient, Position, Progress};
pub use token_info_client::VerifiedOrgsWatcher;

pub struct AppState {
    pub catalog: TokenCatalog,
    /// JIT client for the indexer's GraphQL + progress API.
    pub indexer: IndexerClient,
    /// Verified-orgs allowlist (polled from token-info). Every user-facing
    /// bucket surface filters to these orgs; the indexer itself records all.
    pub verified_orgs: VerifiedOrgsWatcher,
}

impl AppState {
    pub fn new(
        catalog: TokenCatalog,
        indexer_graphql_url: String,
        verified_orgs: VerifiedOrgsWatcher,
    ) -> Self {
        Self {
            catalog,
            indexer: IndexerClient::new(indexer_graphql_url),
            verified_orgs,
        }
    }
}
