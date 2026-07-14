//! Postgres persistence for the solana-token-info catalog.
//!
//! Mirrors the Sui token-info's db module: embedded migrations, an r2d2
//! pool, and a single `Repo` over the one catalog table.

pub mod models;
pub mod schema;

mod repo;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub use repo::Repo;

pub type DbPool = Pool<ConnectionManager<PgConnection>>;

/// Embedded so the binary carries its own migration set; no separate
/// `diesel migration run` step at boot.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/db/migrations");

pub fn establish_pool(database_url: &str, max_size: u32) -> Result<DbPool> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .with_context(|| format!("building r2d2 pool for {database_url}"))
}

pub fn run_migrations(pool: &DbPool) -> Result<()> {
    let mut conn = pool.get().context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running migrations: {e}"))?;
    Ok(())
}
