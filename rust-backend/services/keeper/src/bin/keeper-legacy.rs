//! DEPRECATED (SO-332) — entry point for the retired covered-call vault
//! crank ([`keeper::legacy_vault`]).
//!
//! Deliberately not containerized: `Dockerfile.keeper` copies only the
//! `keeper` binary, and no compose file or workflow references this one.
//! It exists so the crank keeps compiling and stays runnable by hand
//! against an old deployment that still carries the `options_vault`
//! package. New work belongs in the trading-vault pass instead.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("keeper-legacy");
    keeper::legacy_vault::run().await
}
