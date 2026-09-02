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
    /// Normalize Hyperliquid websocket bronze (trades/bbo/ctx) for a UTC
    /// day (default: yesterday).
    Hyperliquid {
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Normalize Deribit chain-snapshot bronze for a UTC day.
    Deribit {
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Normalize Aftermath router quote-ladder bronze (route.*) for a UTC
    /// day into quote_ladder, one partition per pair.
    Aftermath {
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Normalize Bluefin bronze for a UTC day: L2 depth (diffs + REST
    /// snapshots) into book_l2, and funding settlements from the
    /// REST-history poller plus ticker-rollover derivation.
    Bluefin {
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Normalize DeepBook indexer depth snapshots (book.*) for a UTC day
    /// into book_l2.
    Deepbook {
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value_t = 0)]
        lookback_days: u32,
    },
    /// Fetch Deribit DVOL hourly candles into vol_index partitions
    /// (full history is free; re-runs repair gaps).
    Dvol {
        #[arg(long, value_delimiter = ',', default_value = "BTC")]
        currencies: Vec<String>,
        #[arg(long, default_value_t = 3)]
        days: u32,
        #[arg(long)]
        from: Option<String>,
    },
    /// Fetch settled Hyperliquid funding via REST into part-settled
    /// partitions (idempotent; capture gaps self-heal).
    FundingSettled {
        #[arg(long, value_delimiter = ',', default_value = "BTC")]
        coins: Vec<String>,
        /// Days back from today (inclusive) to refresh.
        #[arg(long, default_value_t = 3)]
        days: u32,
        /// Optional explicit start date YYYY-MM-DD (backfill mode).
        #[arg(long)]
        from: Option<String>,
    },
    /// Normalize any vision dump zips without a .done state marker.
    Vision {
        /// "spot" or "um" (USDⓈ-margined futures — perps).
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
        #[arg(long, value_delimiter = ',', default_value = "BTCUSDT")]
        binance_perp_symbols: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "BTC")]
        hyperliquid_coins: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "BTC")]
        deribit_currencies: Vec<String>,
    },
}

fn ws_days(date: Option<String>, lookback_days: u32) -> anyhow::Result<Vec<String>> {
    let end = date.unwrap_or_else(|| {
        (Utc::now() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string()
    });
    let end_day = chrono::NaiveDate::parse_from_str(&end, "%Y-%m-%d")?;
    Ok((0..=lookback_days)
        .map(|b| {
            (end_day - Duration::days(b as i64))
                .format("%Y-%m-%d")
                .to_string()
        })
        .collect())
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
    let store = data_room_store::open(&cli.store_url)?;

    match cli.cmd {
        Cmd::Coinbase {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let n = normalizer::ws::normalize_day(&store, "coinbase", &day).await?;
                tracing::info!(day, streams = n, "coinbase day normalized");
            }
        }
        Cmd::Hyperliquid {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let n = normalizer::ws::normalize_day(&store, "hyperliquid", &day).await?;
                tracing::info!(day, streams = n, "hyperliquid day normalized");
            }
        }
        Cmd::Deribit {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let n = normalizer::deribit::normalize_day(&store, &day).await?;
                tracing::info!(day, streams = n, "deribit day normalized");
            }
        }
        Cmd::Aftermath {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let n = normalizer::aftermath::normalize_day(&store, &day).await?;
                tracing::info!(day, partitions = n, "aftermath day normalized");
            }
        }
        Cmd::Bluefin {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let books = normalizer::book_l2::normalize_day(&store, "bluefin", &day).await?;
                let n = normalizer::bluefin_funding::normalize_day(&store, &day).await?;
                tracing::info!(
                    day,
                    book_l2_partitions = books,
                    funding_parts = n,
                    "bluefin day normalized"
                );
            }
        }
        Cmd::Deepbook {
            date,
            lookback_days,
        } => {
            for day in ws_days(date, lookback_days)? {
                let n = normalizer::book_l2::normalize_day(&store, "deepbook", &day).await?;
                tracing::info!(day, partitions = n, "deepbook day normalized");
            }
        }
        Cmd::Dvol {
            currencies,
            days,
            from,
        } => {
            let to = Utc::now().date_naive();
            let from = match from {
                Some(f) => chrono::NaiveDate::parse_from_str(&f, "%Y-%m-%d")?,
                None => to - Duration::days(days as i64),
            };
            normalizer::deribit::dvol(&store, &currencies, from, to).await?;
        }
        Cmd::FundingSettled { coins, days, from } => {
            let to = Utc::now().date_naive();
            let from = match from {
                Some(f) => chrono::NaiveDate::parse_from_str(&f, "%Y-%m-%d")?,
                None => to - Duration::days(days as i64),
            };
            normalizer::funding::hyperliquid_settled(&store, &coins, from, to).await?;
        }
        Cmd::Vision { market, symbols } => {
            let label = match market.as_str() {
                "spot" => "spot",
                "um" => "um-futures",
                other => anyhow::bail!("unsupported market {other} (want spot|um)"),
            };
            for s in &symbols {
                let n = normalizer::vision::normalize_pending(&store, label, s).await?;
                tracing::info!(symbol = s, zips = n, "vision normalized");
            }
        }
        Cmd::Instruments {
            coinbase_products,
            binance_symbols,
            binance_perp_symbols,
            hyperliquid_coins,
            deribit_currencies,
        } => {
            let today = Utc::now().format("%Y-%m-%d").to_string();
            normalizer::instruments::snapshot(
                &store,
                &coinbase_products,
                &binance_symbols,
                &binance_perp_symbols,
                &hyperliquid_coins,
                &deribit_currencies,
                &today,
            )
            .await?;
        }
    }
    Ok(())
}
