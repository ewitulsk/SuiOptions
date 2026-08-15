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
    /// Sui GraphQL RPC URL. `GET /vaults/:id` does one `object` query against
    /// this for the vault's *live* round state (deployable, open RFQs,
    /// phase, …); a read failure degrades to omitting those fields, never a
    /// 5xx. Defaults to the public testnet endpoint (staging/prod are testnet).
    #[serde(default = "default_sui_graphql_url")]
    pub sui_graphql_url: String,

    /// Data-room lake root for /analytics/* (SO-389), e.g. `s3://<bucket>`.
    /// Optional: unset disables analytics (endpoints return 503). Reads use
    /// the host's IAM role; no reads happen at boot.
    #[serde(default)]
    pub data_room_url: Option<String>,
    /// price-charting read-API base URL (e.g. `http://price-charting:9013`).
    /// When set, the FIFO PnL ledger marks exercises at the option-pool price
    /// at exercise time (SO-209); when unset, exercises are left unpriced.
    #[serde(default)]
    pub price_charting_url: Option<String>,
    /// oracle-service base URL (e.g. `http://oracle-service:9013`) — spot and
    /// realized vol for the `/buckets` strike ladder (SO-400). Unset (or
    /// unreachable) degrades `/buckets` to the buckets that already exist,
    /// never a 5xx.
    #[serde(default)]
    pub oracle_url: Option<String>,
    /// Series families the `/buckets` ladder lists. Empty (the default) means
    /// no synthetic strikes at all — the endpoint then behaves as it did
    /// before the ladder, which is also the correct behaviour on a deployment
    /// predating the any-strike overhaul.
    #[serde(default)]
    pub ladder_pairs: Vec<crate::ladder::LadderPair>,
}

fn default_indexer_graphql_url() -> String {
    "http://127.0.0.1:9002/graphql".to_string()
}

fn default_sui_graphql_url() -> String {
    "https://graphql.testnet.sui.io/graphql".to_string()
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
        assert_eq!(
            cfg.allowed_origins,
            vec!["http://localhost:5173".to_string()]
        );
        assert_eq!(cfg.token_info_url, "http://127.0.0.1:9005");
        // Ladder config is opt-in: a config that predates it still loads.
        assert!(cfg.oracle_url.is_none());
        assert!(cfg.ladder_pairs.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_ladder_pairs_with_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("api-service-ladder-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
bind_addr      = "127.0.0.1:9003"
token_info_url = "http://127.0.0.1:9005"
oracle_url     = "http://127.0.0.1:9013"

[[ladder_pairs]]
underlying = "TBTC"
settlement = "TUSDC"

[[ladder_pairs]]
underlying  = "TSUI"
settlement  = "TUSDC"
option_type = "put"
tick_pct    = 0.05
"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.oracle_url.as_deref(), Some("http://127.0.0.1:9013"));
        assert_eq!(cfg.ladder_pairs.len(), 2);

        let btc = &cfg.ladder_pairs[0];
        assert_eq!(btc.underlying, "TBTC");
        assert!(!btc.is_put(), "option_type defaults to call");
        assert_eq!(btc.tick_pct, 0.025);
        assert_eq!(btc.z_width, 2.5);

        let sui = &cfg.ladder_pairs[1];
        assert!(sui.is_put());
        assert_eq!(sui.tick_pct, 0.05);
        assert_eq!(sui.z_width, 2.5, "unspecified fields still default");
        std::fs::remove_file(&path).ok();
    }
}
