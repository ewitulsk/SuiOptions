//! Postgres persistence for the option-scheduler's local rolls table.
//!
//! Mirrors the indexer's db layout (schema.rs / models.rs / repo fns)
//! but with a single `scheduler_rolls` table that records every roll
//! the scheduler has claimed, submitted, or confirmed.

pub mod models;
pub mod schema;

use std::sync::Arc;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use tracing::debug;

use self::models::{
    NewSchedulerRoll, NewSchedulerVault, RollState, SchedulerRollRow, SchedulerVaultRow, VaultState,
};
use self::schema::{scheduler_rolls, scheduler_vaults};

pub type DbPool = Pool<ConnectionManager<PgConnection>>;
pub type ArcPool = Arc<DbPool>;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub fn establish_pool(database_url: &str, max_size: u32) -> Result<DbPool> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .with_context(|| format!("building scheduler DB pool for {database_url}"))
}

pub fn run_migrations(pool: &DbPool) -> Result<()> {
    let mut conn = pool.get().context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running scheduler migrations: {e}"))?;
    Ok(())
}

/// Highest `expiry_ms` in any active state for a given pair *at a given
/// cadence*, or `None`. Scoped by `expiry_interval_ms` so a pair running two
/// cadences (e.g. weekly + hourly TSUI) doesn't let one family's far expiry
/// suppress the other's roll.
pub fn latest_active_expiry(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_interval_ms: u64,
    product_type: &str,
) -> Result<Option<u64>> {
    use diesel::dsl::max;
    let mut conn = pool.get().context("latest_active_expiry: pool")?;
    let result: Option<i64> = scheduler_rolls::table
        .filter(scheduler_rolls::underlying_symbol.eq(underlying))
        .filter(scheduler_rolls::settlement_symbol.eq(settlement))
        .filter(scheduler_rolls::expiry_interval_ms.eq(expiry_interval_ms as i64))
        .filter(scheduler_rolls::product_type.eq(product_type))
        .filter(
            scheduler_rolls::state.eq_any(&[
                RollState::Pending.as_str(),
                RollState::Submitted.as_str(),
                RollState::Confirmed.as_str(),
                RollState::NeedsReconciliation.as_str(),
            ]),
        )
        .select(max(scheduler_rolls::expiry_ms))
        .first(&mut conn)
        .context("latest_active_expiry: query")?;
    Ok(result.map(|v| v as u64))
}

/// Claim a slot: INSERT a `pending` row. Returns `Ok(true)` if inserted,
/// `Ok(false)` if the partial UNIQUE index blocked us (another path owns
/// this slot).
pub fn claim_slot(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_ms: u64,
    expiry_interval_ms: u64,
    product_type: &str,
    anchor_seq: u64,
) -> Result<bool> {
    let mut conn = pool.get().context("claim_slot: pool")?;
    let new = NewSchedulerRoll {
        underlying_symbol: underlying,
        settlement_symbol: settlement,
        expiry_ms: expiry_ms as i64,
        expiry_interval_ms: expiry_interval_ms as i64,
        product_type,
        state: RollState::Pending.as_str(),
        submit_anchor_seq: Some(anchor_seq as i64),
    };
    let result = diesel::insert_into(scheduler_rolls::table)
        .values(&new)
        .on_conflict_do_nothing()
        .execute(&mut conn);
    match result {
        Ok(n) => {
            debug!(underlying, settlement, expiry_ms, inserted = n, "claim_slot");
            Ok(n > 0)
        }
        Err(e) => {
            // Unique-constraint violation from the partial index also
            // surfaces as a DB error; treat as "slot taken".
            debug!(underlying, settlement, expiry_ms, error = %e, "claim_slot: conflict");
            Ok(false)
        }
    }
}

/// Mark a pending row as submitted with the tx digest and bucket ids.
pub fn mark_submitted(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_ms: u64,
    product_type: &str,
    tx_digest: &str,
    bucket_ids: &[String],
) -> Result<()> {
    let mut conn = pool.get().context("mark_submitted: pool")?;
    let ids_json = serde_json::to_value(bucket_ids).context("serializing bucket_ids")?;
    diesel::update(scheduler_rolls::table)
        .filter(scheduler_rolls::underlying_symbol.eq(underlying))
        .filter(scheduler_rolls::settlement_symbol.eq(settlement))
        .filter(scheduler_rolls::expiry_ms.eq(expiry_ms as i64))
        .filter(scheduler_rolls::product_type.eq(product_type))
        .filter(scheduler_rolls::state.eq(RollState::Pending.as_str()))
        .set((
            scheduler_rolls::state.eq(RollState::Submitted.as_str()),
            scheduler_rolls::tx_digest.eq(tx_digest),
            scheduler_rolls::bucket_ids.eq(ids_json),
            scheduler_rolls::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("mark_submitted: update")?;
    Ok(())
}

/// Mark a pending row as needs_reconciliation (ambiguous failure).
pub fn mark_needs_reconciliation(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_ms: u64,
    product_type: &str,
    error_msg: &str,
) -> Result<()> {
    let mut conn = pool.get().context("mark_needs_reconciliation: pool")?;
    diesel::update(scheduler_rolls::table)
        .filter(scheduler_rolls::underlying_symbol.eq(underlying))
        .filter(scheduler_rolls::settlement_symbol.eq(settlement))
        .filter(scheduler_rolls::expiry_ms.eq(expiry_ms as i64))
        .filter(scheduler_rolls::product_type.eq(product_type))
        .filter(scheduler_rolls::state.eq(RollState::Pending.as_str()))
        .set((
            scheduler_rolls::state.eq(RollState::NeedsReconciliation.as_str()),
            scheduler_rolls::last_error.eq(error_msg),
            scheduler_rolls::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("mark_needs_reconciliation: update")?;
    Ok(())
}

/// Delete a pending row (unambiguous failure — tx never reached consensus).
pub fn delete_pending(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_ms: u64,
    product_type: &str,
) -> Result<()> {
    let mut conn = pool.get().context("delete_pending: pool")?;
    diesel::delete(scheduler_rolls::table)
        .filter(scheduler_rolls::underlying_symbol.eq(underlying))
        .filter(scheduler_rolls::settlement_symbol.eq(settlement))
        .filter(scheduler_rolls::expiry_ms.eq(expiry_ms as i64))
        .filter(scheduler_rolls::product_type.eq(product_type))
        .filter(scheduler_rolls::state.eq(RollState::Pending.as_str()))
        .execute(&mut conn)
        .context("delete_pending")?;
    Ok(())
}

/// Confirm a submitted or needs_reconciliation row via indexer feedback.
pub fn confirm_from_indexer(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    expiry_ms: u64,
    product_type: &str,
    bucket_id: &str,
) -> Result<()> {
    let mut conn = pool.get().context("confirm_from_indexer: pool")?;

    // Append the bucket_id to confirmed_bucket_ids and set state to confirmed.
    let rows: Vec<SchedulerRollRow> = scheduler_rolls::table
        .filter(scheduler_rolls::underlying_symbol.eq(underlying))
        .filter(scheduler_rolls::settlement_symbol.eq(settlement))
        .filter(scheduler_rolls::expiry_ms.eq(expiry_ms as i64))
        .filter(scheduler_rolls::product_type.eq(product_type))
        .filter(
            scheduler_rolls::state
                .eq(RollState::Submitted.as_str())
                .or(scheduler_rolls::state.eq(RollState::NeedsReconciliation.as_str())),
        )
        .load(&mut conn)
        .context("confirm_from_indexer: select")?;

    for row in rows {
        let mut confirmed: Vec<String> = row
            .confirmed_bucket_ids
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !confirmed.contains(&bucket_id.to_string()) {
            confirmed.push(bucket_id.to_string());
        }
        let confirmed_json =
            serde_json::to_value(&confirmed).context("serializing confirmed_bucket_ids")?;
        diesel::update(scheduler_rolls::table.find(row.id))
            .set((
                scheduler_rolls::state.eq(RollState::Confirmed.as_str()),
                scheduler_rolls::confirmed_bucket_ids.eq(confirmed_json),
                scheduler_rolls::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("confirm_from_indexer: update")?;
    }
    Ok(())
}

/// Get all rows in `needs_reconciliation` state.
pub fn needs_reconciliation_rows(pool: &DbPool) -> Result<Vec<SchedulerRollRow>> {
    let mut conn = pool.get().context("needs_reconciliation_rows: pool")?;
    scheduler_rolls::table
        .filter(scheduler_rolls::state.eq(RollState::NeedsReconciliation.as_str()))
        .load(&mut conn)
        .context("needs_reconciliation_rows: query")
}

/// Delete a needs_reconciliation row (reconciler proved the tx never landed).
pub fn delete_reconciled(pool: &DbPool, row_id: i64) -> Result<()> {
    let mut conn = pool.get().context("delete_reconciled: pool")?;
    diesel::delete(scheduler_rolls::table.find(row_id))
        .execute(&mut conn)
        .context("delete_reconciled")?;
    Ok(())
}

/// Confirmed rolls whose expiry is still in the future — the families a vault
/// could still select. The reconciler checks these for on-chain invalidation;
/// past-expiry families age out on their own.
pub fn confirmed_unexpired_rolls(pool: &DbPool, now_ms: u64) -> Result<Vec<SchedulerRollRow>> {
    let mut conn = pool.get().context("confirmed_unexpired_rolls: pool")?;
    scheduler_rolls::table
        .filter(scheduler_rolls::state.eq(RollState::Confirmed.as_str()))
        .filter(scheduler_rolls::expiry_ms.gt(now_ms as i64))
        .load(&mut conn)
        .context("confirmed_unexpired_rolls: query")
}

/// Mark a confirmed roll `superseded` (its on-chain family was fully
/// invalidated). The state is non-active, so it leaves the active slot free
/// for the cadence picker to re-roll a fresh family at the same expiry.
pub fn mark_superseded(pool: &DbPool, row_id: i64) -> Result<()> {
    let mut conn = pool.get().context("mark_superseded: pool")?;
    diesel::update(scheduler_rolls::table.find(row_id))
        .set(scheduler_rolls::state.eq(RollState::Superseded.as_str()))
        .execute(&mut conn)
        .context("mark_superseded")?;
    Ok(())
}

/// Get all active rows (for boot logging).
pub fn all_active_rows(pool: &DbPool) -> Result<Vec<SchedulerRollRow>> {
    let mut conn = pool.get().context("all_active_rows: pool")?;
    scheduler_rolls::table
        .filter(
            scheduler_rolls::state.eq_any(&[
                RollState::Pending.as_str(),
                RollState::Submitted.as_str(),
                RollState::Confirmed.as_str(),
                RollState::NeedsReconciliation.as_str(),
            ]),
        )
        .order(scheduler_rolls::created_at.asc())
        .load(&mut conn)
        .context("all_active_rows: query")
}

// ════════════════════════════ vaults ════════════════════════════════
//
// One row per (underlying, settlement, round_ms) — i.e. per pair *per cadence*.
// The partial UNIQUE index on active states is the hard guard against creating
// a duplicate vault, exactly like `scheduler_rolls_active_slot` does for rolls.
// Keying on `round_ms` lets a weekly and an hourly vault for the same pair
// coexist.

/// The single active vault row for a pair at a given round cadence (the index
/// guarantees ≤ 1), or `None` if the pair has no live vault row at that cadence.
pub fn active_vault_row(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
) -> Result<Option<SchedulerVaultRow>> {
    let mut conn = pool.get().context("active_vault_row: pool")?;
    scheduler_vaults::table
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(
            scheduler_vaults::state.eq_any(&[
                VaultState::Pending.as_str(),
                VaultState::CoinPublished.as_str(),
                VaultState::Confirmed.as_str(),
            ]),
        )
        .first(&mut conn)
        .optional()
        .context("active_vault_row: query")
}

/// Claim a vault slot: INSERT a `pending` row. `Ok(true)` if inserted,
/// `Ok(false)` if the partial UNIQUE index blocked us (the pair already has a
/// live vault row).
pub fn claim_vault_slot(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
) -> Result<bool> {
    let mut conn = pool.get().context("claim_vault_slot: pool")?;
    let new = NewSchedulerVault {
        underlying_symbol: underlying,
        settlement_symbol: settlement,
        round_ms: round_ms as i64,
        state: VaultState::Pending.as_str(),
    };
    let result = diesel::insert_into(scheduler_vaults::table)
        .values(&new)
        .on_conflict_do_nothing()
        .execute(&mut conn);
    match result {
        Ok(n) => {
            debug!(underlying, settlement, inserted = n, "claim_vault_slot");
            Ok(n > 0)
        }
        Err(e) => {
            debug!(underlying, settlement, error = %e, "claim_vault_slot: conflict");
            Ok(false)
        }
    }
}

/// Record the published share coin: move the pair's `pending` row to
/// `coin_published`, stashing the package, the VShare type, and the harvested
/// `TreasuryCap` id so a later pass can resume `create_vault` without
/// re-publishing.
pub fn mark_vault_coin_published(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
    package: &str,
    coin_type: &str,
    cap_id: &str,
    publish_digest: &str,
) -> Result<()> {
    let mut conn = pool.get().context("mark_vault_coin_published: pool")?;
    diesel::update(scheduler_vaults::table)
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(scheduler_vaults::state.eq(VaultState::Pending.as_str()))
        .set((
            scheduler_vaults::state.eq(VaultState::CoinPublished.as_str()),
            scheduler_vaults::share_coin_package.eq(package),
            scheduler_vaults::share_coin_type.eq(coin_type),
            scheduler_vaults::share_cap_id.eq(cap_id),
            scheduler_vaults::publish_digest.eq(publish_digest),
            scheduler_vaults::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("mark_vault_coin_published: update")?;
    Ok(())
}

/// Move the pair's active row to `confirmed` once `create_vault` lands,
/// recording the vault id and tx digest.
pub fn mark_vault_confirmed(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
    vault_id: &str,
    create_digest: &str,
) -> Result<()> {
    let mut conn = pool.get().context("mark_vault_confirmed: pool")?;
    diesel::update(scheduler_vaults::table)
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(
            scheduler_vaults::state
                .eq(VaultState::Pending.as_str())
                .or(scheduler_vaults::state.eq(VaultState::CoinPublished.as_str())),
        )
        .set((
            scheduler_vaults::state.eq(VaultState::Confirmed.as_str()),
            scheduler_vaults::vault_id.eq(vault_id),
            scheduler_vaults::create_digest.eq(create_digest),
            scheduler_vaults::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("mark_vault_confirmed: update")?;
    Ok(())
}

/// Reconcile a vault the indexer reports but our DB has no confirmed row for
/// (e.g. the scheduler DB was wiped while the chain kept the vault). Update an
/// existing active row to `confirmed`, or insert a fresh confirmed row. Either
/// way the pair is never re-created.
pub fn record_existing_vault(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
    vault_id: &str,
) -> Result<()> {
    let mut conn = pool.get().context("record_existing_vault: pool")?;
    let updated = diesel::update(scheduler_vaults::table)
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(
            scheduler_vaults::state.eq_any(&[
                VaultState::Pending.as_str(),
                VaultState::CoinPublished.as_str(),
            ]),
        )
        .set((
            scheduler_vaults::state.eq(VaultState::Confirmed.as_str()),
            scheduler_vaults::vault_id.eq(vault_id),
            scheduler_vaults::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("record_existing_vault: update")?;
    if updated == 0 {
        let new = NewSchedulerVault {
            underlying_symbol: underlying,
            settlement_symbol: settlement,
            round_ms: round_ms as i64,
            state: VaultState::Confirmed.as_str(),
        };
        diesel::insert_into(scheduler_vaults::table)
            .values(&new)
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .context("record_existing_vault: insert")?;
        diesel::update(scheduler_vaults::table)
            .filter(scheduler_vaults::underlying_symbol.eq(underlying))
            .filter(scheduler_vaults::settlement_symbol.eq(settlement))
            .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
            .filter(scheduler_vaults::state.eq(VaultState::Confirmed.as_str()))
            .filter(scheduler_vaults::vault_id.is_null())
            .set(scheduler_vaults::vault_id.eq(vault_id))
            .execute(&mut conn)
            .context("record_existing_vault: backfill id")?;
    }
    Ok(())
}

/// Retire the active row whose vault was paused (decommissioned) on-chain:
/// move it out of the active-slot index so the scheduler rolls a fresh
/// replacement for the pair (hard cutover). Matched by `vault_id` so only the
/// paused vault's own row is retired — a row already pointing at a live
/// replacement is left untouched. Returns the number of rows retired.
pub fn retire_paused_vault(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
    vault_id: &str,
) -> Result<usize> {
    let mut conn = pool.get().context("retire_paused_vault: pool")?;
    diesel::update(scheduler_vaults::table)
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(scheduler_vaults::vault_id.eq(vault_id))
        .filter(scheduler_vaults::state.eq(VaultState::Confirmed.as_str()))
        .set((
            scheduler_vaults::state.eq(VaultState::Retired.as_str()),
            scheduler_vaults::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("retire_paused_vault: update")
}

/// Give up on the pair's active create attempt: move the row to `failed` so a
/// later pass can retry with a fresh `pending` claim. Used for unambiguous
/// failures (build/preflight/revert) where the tx never landed.
pub fn mark_vault_failed(
    pool: &DbPool,
    underlying: &str,
    settlement: &str,
    round_ms: u64,
    error_msg: &str,
) -> Result<()> {
    let mut conn = pool.get().context("mark_vault_failed: pool")?;
    diesel::update(scheduler_vaults::table)
        .filter(scheduler_vaults::underlying_symbol.eq(underlying))
        .filter(scheduler_vaults::settlement_symbol.eq(settlement))
        .filter(scheduler_vaults::round_ms.eq(round_ms as i64))
        .filter(
            scheduler_vaults::state
                .eq(VaultState::Pending.as_str())
                .or(scheduler_vaults::state.eq(VaultState::CoinPublished.as_str())),
        )
        .set((
            scheduler_vaults::state.eq(VaultState::Failed.as_str()),
            scheduler_vaults::last_error.eq(error_msg),
            scheduler_vaults::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut conn)
        .context("mark_vault_failed: update")?;
    Ok(())
}

/// All active vault rows, for boot logging.
pub fn all_active_vault_rows(pool: &DbPool) -> Result<Vec<SchedulerVaultRow>> {
    let mut conn = pool.get().context("all_active_vault_rows: pool")?;
    scheduler_vaults::table
        .filter(
            scheduler_vaults::state.eq_any(&[
                VaultState::Pending.as_str(),
                VaultState::CoinPublished.as_str(),
                VaultState::Confirmed.as_str(),
            ]),
        )
        .order(scheduler_vaults::created_at.asc())
        .load(&mut conn)
        .context("all_active_vault_rows: query")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a throwaway pool connected to the DB at
    /// `SCHEDULER_TEST_DATABASE_URL`.  All `#[ignore]` tests need this
    /// env var pointing at a real Postgres.
    fn test_pool() -> DbPool {
        let url = std::env::var("SCHEDULER_TEST_DATABASE_URL")
            .expect("set SCHEDULER_TEST_DATABASE_URL to run DB tests");
        let pool = establish_pool(&url, 2).expect("pool");
        run_migrations(&pool).expect("migrations");
        // Truncate between tests so they are independent.
        let mut conn = pool.get().unwrap();
        diesel::sql_query("TRUNCATE scheduler_rolls RESTART IDENTITY")
            .execute(&mut conn)
            .expect("truncate");
        pool
    }

    #[test]
    fn roll_state_round_trip() {
        for state in &[
            RollState::Pending,
            RollState::Submitted,
            RollState::Confirmed,
            RollState::NeedsReconciliation,
        ] {
            assert_eq!(RollState::parse(state.as_str()), Some(*state));
            assert!(state.is_active());
        }
        // Superseded round-trips but is NOT active — that's what frees the slot.
        assert_eq!(
            RollState::parse(RollState::Superseded.as_str()),
            Some(RollState::Superseded)
        );
        assert!(!RollState::Superseded.is_active());
        assert_eq!(RollState::parse("bogus"), None);
    }

    const WEEK: u64 = 604_800_000;
    const HOUR: u64 = 3_600_000;

    #[test]
    #[ignore] // requires SCHEDULER_TEST_DATABASE_URL
    fn claim_and_duplicate_blocked() {
        let pool = test_pool();
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 1_000, WEEK, "call", 0).unwrap());
        // Same slot: partial UNIQUE index blocks duplicates.
        assert!(!claim_slot(&pool, "TBTC", "TUSDC", 1_000, WEEK, "call", 0).unwrap());
        // ...but a put at the same slot is a distinct product — allowed.
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 1_000, WEEK, "put", 0).unwrap());
    }

    #[test]
    #[ignore]
    fn claim_allowed_after_delete() {
        let pool = test_pool();
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 2_000, WEEK, "call", 0).unwrap());
        delete_pending(&pool, "TBTC", "TUSDC", 2_000, "call").unwrap();
        // Row is gone; re-claim succeeds.
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 2_000, WEEK, "call", 0).unwrap());
    }

    #[test]
    #[ignore]
    fn superseded_frees_slot_for_reroll() {
        let pool = test_pool();
        let now = 1_000_000u64;
        // Confirmed family at a future expiry.
        assert!(claim_slot(&pool, "TSUI", "TUSDC", now + HOUR, HOUR, "call", 0).unwrap());
        mark_submitted(&pool, "TSUI", "TUSDC", now + HOUR, "call", "0xabc", &["id1".into()]).unwrap();
        confirm_from_indexer(&pool, "TSUI", "TUSDC", now + HOUR, "call", "id1").unwrap();

        // It's a re-roll candidate, and still blocks a duplicate claim.
        let cands = confirmed_unexpired_rolls(&pool, now).unwrap();
        assert_eq!(cands.len(), 1);
        assert!(!claim_slot(&pool, "TSUI", "TUSDC", now + HOUR, HOUR, "call", 0).unwrap());

        // Supersede (family fully invalidated) → slot frees, latest drops it,
        // and the same expiry can be re-rolled.
        mark_superseded(&pool, cands[0].id).unwrap();
        assert_eq!(latest_active_expiry(&pool, "TSUI", "TUSDC", HOUR, "call").unwrap(), None);
        assert!(confirmed_unexpired_rolls(&pool, now).unwrap().is_empty());
        assert!(claim_slot(&pool, "TSUI", "TUSDC", now + HOUR, HOUR, "call", 0).unwrap());
    }

    #[test]
    #[ignore]
    fn submit_and_confirm_workflow() {
        let pool = test_pool();
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 3_000, WEEK, "call", 42).unwrap());
        mark_submitted(&pool, "TBTC", "TUSDC", 3_000, "call", "0xabc", &["id1".into()]).unwrap();
        confirm_from_indexer(&pool, "TBTC", "TUSDC", 3_000, "call", "id1").unwrap();
        let rows = all_active_rows(&pool).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "confirmed");
    }

    #[test]
    #[ignore]
    fn reconciliation_workflow() {
        let pool = test_pool();
        assert!(claim_slot(&pool, "TBTC", "TUSDC", 4_000, WEEK, "call", 10).unwrap());
        mark_needs_reconciliation(&pool, "TBTC", "TUSDC", 4_000, "call", "timeout").unwrap();
        let rows = needs_reconciliation_rows(&pool).unwrap();
        assert_eq!(rows.len(), 1);
        delete_reconciled(&pool, rows[0].id).unwrap();
        assert!(needs_reconciliation_rows(&pool).unwrap().is_empty());
    }

    #[test]
    #[ignore]
    fn latest_active_expiry_picks_highest() {
        let pool = test_pool();
        claim_slot(&pool, "TBTC", "TUSDC", 1_000, WEEK, "call", 0).unwrap();
        claim_slot(&pool, "TBTC", "TUSDC", 2_000, WEEK, "call", 0).unwrap();
        assert_eq!(
            latest_active_expiry(&pool, "TBTC", "TUSDC", WEEK, "call").unwrap(),
            Some(2_000)
        );
    }

    #[test]
    #[ignore]
    fn latest_active_expiry_is_cadence_scoped() {
        // A weekly family with a far expiry must NOT suppress the hourly
        // family's much nearer latest expiry for the same pair.
        let pool = test_pool();
        claim_slot(&pool, "TSUI", "TUSDC", 10 * WEEK, WEEK, "call", 0).unwrap();
        claim_slot(&pool, "TSUI", "TUSDC", 5 * HOUR, HOUR, "call", 0).unwrap();
        assert_eq!(
            latest_active_expiry(&pool, "TSUI", "TUSDC", WEEK, "call").unwrap(),
            Some(10 * WEEK)
        );
        assert_eq!(
            latest_active_expiry(&pool, "TSUI", "TUSDC", HOUR, "call").unwrap(),
            Some(5 * HOUR)
        );
    }
}
