//! Shared application state.
//!
//! solana-api-service holds no protocol state of its own: every read is a
//! just-in-time GraphQL query to solana-indexer (see
//! [`solana_indexer_graphql`]). This struct is just the token catalog plus
//! the indexer client.

use crate::catalog::TokenCatalog;

pub use solana_indexer_graphql::{
    Auction, AuctionBid, Bucket as IndexerBucket, IndexedEvent, IndexerClient, Position, Progress,
    Vault, VaultRound,
};

pub struct AppState {
    pub catalog: TokenCatalog,
    /// JIT client for solana-indexer's GraphQL + progress API.
    pub indexer: IndexerClient,
    /// Predicted-APY read-API base URL (solana-price-charting's
    /// `/vault-apy/:id`). `None` on envs that don't run it — the apy
    /// endpoint then serves realized only.
    pub derived_metrics_url: Option<String>,
    /// Solana JSON-RPC URL for the live-vault `getAccountInfo` read.
    pub solana_rpc_url: String,
    /// solana-price-charting read-API base URL. Used to mark exercises at
    /// the option price at exercise time in the FIFO PnL ledger. `None`
    /// (or no data) → exercises are marked at the bucket strike.
    pub price_charting_url: Option<String>,
    /// Shared HTTP client for the worker read API + the RPC read.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(
        catalog: TokenCatalog,
        indexer_graphql_url: String,
        derived_metrics_url: Option<String>,
        solana_rpc_url: String,
        price_charting_url: Option<String>,
    ) -> Self {
        Self {
            catalog,
            indexer: IndexerClient::new(indexer_graphql_url),
            derived_metrics_url: derived_metrics_url.map(|u| u.trim_end_matches('/').to_string()),
            solana_rpc_url,
            price_charting_url: price_charting_url.map(|u| u.trim_end_matches('/').to_string()),
            http: reqwest::Client::new(),
        }
    }
}
