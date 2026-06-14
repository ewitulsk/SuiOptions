//! Postgres persistence for predicted-APY snapshots.
//!
//! Embedded migrations run at boot (`run_migrations`), so a freshly-wiped DB
//! (e.g. after a contract redeploy reset its schema) rebuilds itself on start.

pub mod models;
pub mod schema;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::upsert::excluded;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

use models::{NewPrediction, PredictionRow};
use schema::vault_predictions;

pub type DbPool = Pool<ConnectionManager<PgConnection>>;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub fn establish_pool(database_url: &str, max_size: u32) -> Result<DbPool> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .with_context(|| format!("building derived-metrics DB pool for {database_url}"))
}

pub fn run_migrations(pool: &DbPool) -> Result<()> {
    let mut conn = pool.get().context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running derived-metrics migrations: {e}"))?;
    Ok(())
}

/// Upsert a vault's predicted-APY points (latest snapshot wins per
/// (vault, kind, horizon)).
pub fn upsert_predictions(pool: &DbPool, rows: &[NewPrediction]) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut conn = pool.get().context("checking out connection for upsert")?;
    let n = diesel::insert_into(vault_predictions::table)
        .values(rows)
        .on_conflict((
            vault_predictions::vault_id,
            vault_predictions::kind,
            vault_predictions::horizon,
        ))
        .do_update()
        .set((
            vault_predictions::t_ms.eq(excluded(vault_predictions::t_ms)),
            vault_predictions::apy.eq(excluded(vault_predictions::apy)),
            vault_predictions::confidence.eq(excluded(vault_predictions::confidence)),
            vault_predictions::computed_at.eq(excluded(vault_predictions::computed_at)),
        ))
        .execute(&mut conn)
        .context("upserting vault_predictions")?;
    Ok(n)
}

/// All predicted points for one vault, ordered (kind, horizon).
pub fn predictions_for(pool: &DbPool, vault_id: &str) -> Result<Vec<PredictionRow>> {
    let mut conn = pool.get().context("checking out connection for read")?;
    vault_predictions::table
        .filter(vault_predictions::vault_id.eq(vault_id))
        .order((vault_predictions::kind.asc(), vault_predictions::horizon.asc()))
        .load::<PredictionRow>(&mut conn)
        .context("loading vault_predictions")
}
