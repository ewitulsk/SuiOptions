//! Postgres persistence.
//!
//! One transaction per applied slot ([`Repo::apply_slot`]): insert the
//! decoded events into the append-only log, fold the materialised views,
//! advance `indexer_progress`. Idempotency is structural — every view fold
//! is gated on the event's `UNIQUE (signature, inner_ix_index)` insert
//! actually landing, so replays (fromSlot resume, backfill overlap) are
//! no-ops all the way down. There is no in-memory store to drift.

pub mod models;
pub mod schema;

mod repo;

use std::sync::Arc;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub use repo::{AuctionQuery, BucketQuery, EventFilter, EventQuery, PendingEvent, Repo, SlotBatch};

pub type DbPool = Pool<ConnectionManager<PgConnection>>;
pub type ArcPool = Arc<DbPool>;

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
    let mut conn = pool
        .get()
        .context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running migrations: {e}"))?;
    Ok(())
}
