//! Retail writer CLI — stands in for the off-chain frontend. See
//! [`writer::run`] for the full §8.1 flow; this binary is a thin clap
//! shim that calls it and prints the resulting digest.

use anyhow::Result;
use clap::Parser;

use writer::{run, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let outcome = run(cli).await?;
    println!("✓ execute_write digest: {}", outcome.digest);
    Ok(())
}
