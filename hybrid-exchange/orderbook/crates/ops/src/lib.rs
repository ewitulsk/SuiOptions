//! Config, tracing and metrics (spec §5.1 `ops`).

use orderbook_core::Market;
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {0}: {1}")]
    Invalid(&'static str, String),
    #[error("markets file: {0}")]
    Markets(String),
}

/// Service configuration, environment-driven.
#[derive(Clone, Debug)]
pub struct Config {
    /// Sui JSON-RPC endpoint.
    pub rpc_url: String,
    /// Postgres connection string.
    pub database_url: String,
    /// REST/WS bind address.
    pub bind: SocketAddr,
    /// Prometheus exporter bind address (None disables).
    pub metrics_bind: Option<SocketAddr>,
    /// Published exchange package ID.
    pub package_id: String,
    /// Relayer ed25519 secret key, hex-encoded 32-byte seed. Optional: with
    /// no key the service runs in open-orderbook-only mode (no matched
    /// settlement submission).
    pub relayer_seed_hex: Option<String>,
    /// Path to the markets JSON file (array of `Market`).
    pub markets_file: String,
}

fn var(name: &'static str) -> Result<String, ConfigError> {
    std::env::var(name).map_err(|_| ConfigError::Missing(name))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind = std::env::var("ORDERBOOK_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".into());
        let metrics_bind = std::env::var("ORDERBOOK_METRICS_BIND").ok();
        Ok(Config {
            rpc_url: var("SUI_RPC_URL")?,
            database_url: var("DATABASE_URL")?,
            bind: bind
                .parse()
                .map_err(|e: std::net::AddrParseError| ConfigError::Invalid("ORDERBOOK_BIND", e.to_string()))?,
            metrics_bind: metrics_bind
                .map(|s| {
                    s.parse().map_err(|e: std::net::AddrParseError| {
                        ConfigError::Invalid("ORDERBOOK_METRICS_BIND", e.to_string())
                    })
                })
                .transpose()?,
            package_id: var("EXCHANGE_PACKAGE_ID")?,
            relayer_seed_hex: std::env::var("RELAYER_SEED_HEX").ok(),
            markets_file: std::env::var("MARKETS_FILE").unwrap_or_else(|_| "markets.json".into()),
        })
    }

    pub fn load_markets(&self) -> Result<Vec<Market>, ConfigError> {
        let raw = std::fs::read_to_string(&self.markets_file)
            .map_err(|e| ConfigError::Markets(format!("{}: {e}", self.markets_file)))?;
        let markets: Vec<MarketFile> =
            serde_json::from_str(&raw).map_err(|e| ConfigError::Markets(e.to_string()))?;
        Ok(markets.into_iter().map(Into::into).collect())
    }
}

/// On-disk market entry (same shape as `Market`, aliased for clarity).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketFile {
    symbol: String,
    registry_id: orderbook_core::SuiAddress,
    base: String,
    quote: String,
    tick_size: u64,
    min_size: u64,
    lot_size: u64,
    #[serde(default)]
    current_fee_bps: u64,
}

impl From<MarketFile> for Market {
    fn from(m: MarketFile) -> Market {
        Market {
            symbol: m.symbol,
            registry_id: m.registry_id,
            base: m.base,
            quote: m.quote,
            tick_size: m.tick_size,
            min_size: m.min_size,
            lot_size: m.lot_size,
            current_fee_bps: m.current_fee_bps,
        }
    }
}

/// Install tracing (env-filter) and the Prometheus exporter.
pub fn init_telemetry(metrics_bind: Option<SocketAddr>) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Some(addr) = metrics_bind {
        if let Err(e) = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
        {
            tracing::warn!(error = %e, "failed to install prometheus exporter");
        }
    }
}
