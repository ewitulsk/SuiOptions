use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// HTTP bind address (frontend hits this).
    pub bind_addr: SocketAddr,
    /// solana-indexer GraphQL query endpoint. All protocol reads are JIT
    /// queries against this. e.g. `http://127.0.0.1:9002/graphql`.
    #[serde(default = "default_indexer_graphql_url")]
    pub indexer_graphql_url: String,
    /// CORS allow-list. `["*"]` permits any origin (dev only).
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,
    /// solana-token-info public base URL. The mint → {symbol, decimals}
    /// catalog is fetched from here at boot (hard cutover — no
    /// solana-deployments.json fallback).
    pub token_info_url: String,
    /// Predicted-APY read-API base URL — solana-price-charting's
    /// `/vault-apy/:id` (e.g. `http://solana-price-charting:9011`). When
    /// unset, `/vaults/:id/apy` serves realized points only and an empty
    /// predicted series.
    #[serde(default)]
    pub derived_metrics_url: Option<String>,
    /// Solana JSON-RPC URL. `GET /vaults/:id` does one `getAccountInfo`
    /// against this for the vault's *live* round state (phase, open RFQs,
    /// selling window, config guardrails); a read failure degrades to
    /// omitting those fields, never a 5xx. Defaults to the public devnet
    /// endpoint (staging/prod target devnet for now).
    #[serde(default = "default_solana_rpc_url")]
    pub solana_rpc_url: String,
    /// solana-price-charting read-API base URL. When set, the FIFO PnL
    /// ledger marks exercises at the option price at exercise time; when
    /// unset (or no data — always, until a Solana DEX integration lands)
    /// exercises are marked at the bucket strike.
    #[serde(default)]
    pub price_charting_url: Option<String>,
}

fn default_indexer_graphql_url() -> String {
    "http://127.0.0.1:9002/graphql".to_string()
}

fn default_solana_rpc_url() -> String {
    "https://api.devnet.solana.com".to_string()
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "solana-api-service-config-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
bind_addr           = "127.0.0.1:9003"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
allowed_origins     = ["http://localhost:5173"]
token_info_url      = "http://127.0.0.1:9005"
"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9003");
        assert_eq!(cfg.indexer_graphql_url, "http://127.0.0.1:9002/graphql");
        assert_eq!(cfg.allowed_origins, vec!["http://localhost:5173".to_string()]);
        assert_eq!(cfg.token_info_url, "http://127.0.0.1:9005");
        assert_eq!(cfg.solana_rpc_url, "https://api.devnet.solana.com");
        assert!(cfg.derived_metrics_url.is_none());
        assert!(cfg.price_charting_url.is_none());
        std::fs::remove_file(&path).ok();
    }
}
