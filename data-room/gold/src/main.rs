use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    /// s3://bucket or file:///path lake root.
    #[arg(long, env = "STORE_URL", global = true, default_value = "")]
    store_url: String,
    /// YYYY-MM-DD; default yesterday UTC.
    #[arg(long, global = true)]
    date: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Capture-gap ledger from bronze markers.
    Gaps,
    /// OHLCV bars from silver trades.
    Bars,
    /// Realized-vol grid (spec §8).
    Rv,
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
    let date = cli.date.unwrap_or_else(|| {
        (Utc::now() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string()
    });

    match cli.cmd {
        Cmd::Gaps => gold::gaps::compute_day(&store, &date).await?,
        Cmd::Bars => gold::bars::compute_day(&store, &date).await?,
        Cmd::Rv => gold::rv::compute_day(&store, &date).await?,
    };
    Ok(())
}
