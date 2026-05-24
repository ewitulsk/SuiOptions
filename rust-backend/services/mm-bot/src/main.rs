//! Market-maker bot. See [`mm_bot::run`] for the lifecycle docs — the
//! body of the binary now lives in the library so the chain test harness
//! can drive the real bot in-process.

use anyhow::Result;
use clap::Parser;

use mm_bot::{run, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    shared::logging::init();
    let cli = Cli::parse();
    run(cli).await
}
