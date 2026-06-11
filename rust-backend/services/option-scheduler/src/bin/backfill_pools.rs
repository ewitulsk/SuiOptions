//! One-off admin tool (SO-171): create DeepBook permissionless pools for
//! buckets that lack one.
//!
//! The scheduler now creates a pool for every NEW roll; this backfills
//! pre-existing buckets (and any pool a roll failed to create). Pool creation
//! is idempotent at the contract level — a pool that already exists reverts at
//! dry-run and is reported as a skip, not a hard failure.
//!
//! The base side of each pool is the bucket's Call coin (which inherits the
//! underlying's decimals); the quote side is the shared settlement asset. Pass
//! the call coin types to backfill and the settlement symbol + the call
//! decimals (= the underlying's decimals for that pair).
//!
//! Example:
//!   backfill_pools --network testnet \
//!     --token-info-url https://sui-options.com/staging/token-info \
//!     --settlement TUSDC --base-decimals 8 \
//!     --call-types 0x..::call_0::CALL_0,0x..::call_3::CALL_3

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{create_pool, DeepBookHandles};
use token_info_client::TokenInfoClient;

#[derive(Parser, Debug)]
#[command(
    name = "backfill_pools",
    about = "Create DeepBook pools for pool-less buckets (SO-171)"
)]
struct Cli {
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    token_info_url: String,

    #[arg(
        short = 's',
        long,
        default_value = "services/option-scheduler/config/secrets.toml"
    )]
    secrets: std::path::PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    gas_budget: u64,

    /// Settlement token symbol (the pool's quote side), looked up in token-info
    /// for its coin type and decimals.
    #[arg(long)]
    settlement: String,

    /// Decimals of the call coin (the pool's base side) — same as the
    /// underlying's decimals for the pair.
    #[arg(long)]
    base_decimals: u8,

    /// Comma-separated Call coin types to create pools for.
    #[arg(long, value_delimiter = ',')]
    call_types: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let call_types: Vec<String> = cli
        .call_types
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if call_types.is_empty() {
        anyhow::bail!("--call-types is required (comma-separated call coin types)");
    }

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cli.token_info_url))?;

    let db = snapshot
        .deepbook()
        .context("token-info reports no DeepBook deployment for this network")?;
    let handles = DeepBookHandles {
        package: db.package()?,
        original_package: db.original_package()?,
        registry: db.registry()?,
    };
    let deep_coin_type = db.deep_coin_type.clone();
    let fee = db.pool_creation_fee_units()?;

    let s_spec = snapshot
        .token_spec(&cli.settlement)
        .with_context(|| format!("settlement {} not in token-info catalog", cli.settlement))?;
    let settlement_type = s_spec.coin_type.clone();
    let quote_decimals = s_spec.decimals;

    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    info!(
        signer = %wrap.signer.address,
        settlement = %settlement_type,
        base_decimals = cli.base_decimals,
        quote_decimals,
        pools = call_types.len(),
        "backfilling DeepBook pools"
    );

    let (mut created, mut skipped) = (0usize, 0usize);
    for call_type in &call_types {
        match create_pool(
            &wrap.client,
            &wrap.signer,
            &handles,
            &deep_coin_type,
            fee,
            call_type,
            &settlement_type,
            cli.base_decimals,
            quote_decimals,
            cli.gas_budget,
        )
        .await
        {
            Ok(pool) => {
                info!(%call_type, %pool, "pool created");
                created += 1;
            }
            Err(e) => {
                warn!(
                    error = %format!("{e:#}"),
                    %call_type,
                    "pool creation failed (may already exist — check the revert reason)"
                );
                skipped += 1;
            }
        }
    }
    info!(created, skipped, "backfill complete");
    Ok(())
}
