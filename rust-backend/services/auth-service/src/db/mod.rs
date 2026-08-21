//! Postgres persistence for the identity store. Same diesel + r2d2 +
//! embedded-migration shape as the indexer / cctp-relay `db` modules.

pub mod models;
pub mod repo;
pub mod schema;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub type DbPool = Pool<ConnectionManager<PgConnection>>;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/db/migrations");

pub fn establish_pool(database_url: &str, max_size: u32) -> Result<DbPool> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .context("building r2d2 pool for the auth-service DB")
}

pub fn run_migrations(pool: &DbPool) -> Result<()> {
    let mut conn = pool.get().context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running migrations: {e}"))?;
    Ok(())
}
