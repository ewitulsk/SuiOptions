//! Postgres persistence for the indexer.
//!
//! The in-memory `Store` is now a cache over this layer: the worker writes
//! one transaction per checkpoint (see [`Repo::apply_checkpoint`]) and the
//! boot path hydrates the materialised views via [`Repo::hydrate`].
//!
//! Mirrors Pismo's split (`schema.rs` / `models.rs` / repository) but with
//! one consolidated `Repo` instead of one repo per event table — every event
//! we care about goes into `indexed_events`, and the per-bucket/account view
//! tables are a fixed set of four.

pub mod models;
pub mod schema;

mod repo;

use std::sync::Arc;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub use repo::{CheckpointBatch, EventBuild, EventFilter, EventQuery, HydratedViews, Repo};

pub type DbPool = Pool<ConnectionManager<PgConnection>>;
pub type ArcPool = Arc<DbPool>;

/// Embedded so the binary carries its own migration set; no separate
/// `diesel migration run` step required at boot.
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
