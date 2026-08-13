use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    /// s3://bucket or file:///path lake root.
    #[arg(long, env = "STORE_URL", global = true, default_value = "")]
    store_url: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Normalize Coinbase websocket bronze for a UTC day (default:
    /// yesterday), plus a re-run window for late uploads.
    Coinbase {
        /// YYYY-MM-DD; default yesterday UTC.
        #[arg(long)]
        date: Option<String>,
        /// Also re-normalize this many prior days (idempotent overwrite).
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Normalize any vision dump zips without a .done state marker.
    Vision {
        #[arg(long, default_value = "spot")]
        market: String,
        #[arg(long, value_delimiter = ',', default_value = "BTCUSDC")]
        symbols: Vec<String>,
    },
    /// Write today's instrument-master snapshot.
    Instruments {
        #[arg(long, value_delimiter = ',', default_value = "BTC-USD")]
        coinbase_products: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "BTCUSDC")]
        binance_symbols: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    anyhow::ensure!(
        !cli.store_url.is_empty(),
        "--store-url or STORE_URL required"
    );
    let store = store::open(&cli.store_url)?;

    match cli.cmd {
        Cmd::Coinbase {
            date,
            lookback_days,
        } => {
            let end = date.unwrap_or_else(|| {
                (Utc::now() - Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string()
            });
            let end_day = chrono::NaiveDate::parse_from_str(&end, "%Y-%m-%d")?;
            for back in 0..=lookback_days {
                let day = (end_day - Duration::days(back as i64))
                    .format("%Y-%m-%d")
                    .to_string();
                let n = normalizer::coinbase::normalize_day(&store, &day).await?;
                tracing::info!(day, streams = n, "coinbase day normalized");
            }
        }
        Cmd::Vision { market, symbols } => {
            for s in &symbols {
                let n = normalizer::vision::normalize_pending(&store, &market, s).await?;
                tracing::info!(symbol = s, zips = n, "vision normalized");
            }
        }
        Cmd::Instruments {
            coinbase_products,
            binance_symbols,
        } => {
            let today = Utc::now().format("%Y-%m-%d").to_string();
            normalizer::instruments::snapshot(&store, &coinbase_products, &binance_symbols, &today)
                .await?;
        }
    }
    Ok(())
}
