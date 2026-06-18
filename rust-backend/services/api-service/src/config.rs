use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// HTTP bind address (frontend hits this).
    pub bind_addr: SocketAddr,
    /// Indexer GraphQL query endpoint. All protocol reads are JIT queries
    /// against this. e.g. `http://127.0.0.1:9002/graphql`.
    #[serde(default = "default_indexer_graphql_url")]
    pub indexer_graphql_url: String,
    /// CORS allow-list. `["*"]` permits any origin (dev only).
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,
    /// token-info public base URL. The coin-type → {symbol, decimals} catalog
    /// is fetched from here at boot (replaces reading `deployments.json`).
    pub token_info_url: String,
    /// Predicted-APY read-API base URL — now price-charting's `/vault-apy/:id`
    /// (e.g. `http://price-charting:9011`), after the derived-metric-worker was
    /// folded into it. When unset, `/vaults/:id/apy` serves realized points
    /// only and an empty predicted series.
    #[serde(default)]
    pub derived_metrics_url: Option<String>,
    /// Sui fullnode JSON-RPC URL. `GET /vaults/:id` does one `sui_getObject`
    /// against this for the vault's *live* round state (deployable, open RFQs,
    /// phase, …); a read failure degrades to omitting those fields, never a
    /// 5xx. Defaults to the public testnet fullnode (staging/prod are testnet).
    #[serde(default = "default_sui_rpc_url")]
    pub sui_rpc_url: String,
    /// price-charting read-API base URL (e.g. `http://price-charting:9013`).
    /// When set, the FIFO PnL ledger marks exercises at the option-pool price
    /// at exercise time (SO-209); when unset, exercises are left unpriced.
    #[serde(default)]
    pub price_charting_url: Option<String>,
}

fn default_indexer_graphql_url() -> String {
    "http://127.0.0.1:9002/graphql".to_string()
}

fn default_sui_rpc_url() -> String {
    "https://fullnode.testnet.sui.io:443".to_string()
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
        let path = dir.join(format!("api-service-config-{}.toml", std::process::id()));
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
        std::fs::remove_file(&path).ok();
    }
}
