//! Postgres-facing repository for the durable supported-token catalog.
//!
//! This is the operator-managed catalog only. On non-mainnet-beta networks
//! the `/tokens` endpoint additionally overlays test tokens at read time
//! (see `overlay.rs`) — those never touch this table.

use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::upsert::excluded;

use super::models::{TokenRow, UpsertToken};
use super::schema::supported_tokens as st;
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: std::sync::Arc<DbPool>,
}

impl Repo {
    pub fn new(pool: std::sync::Arc<DbPool>) -> Self {
        Self { pool }
    }

    /// All catalog rows, ticker-sorted. `enabled_only` filters to enabled.
    pub fn list(&self, enabled_only: bool) -> Result<Vec<TokenRow>> {
        let mut conn = self.pool.get().context("checkout conn")?;
        let mut q = st::table.into_boxed();
        if enabled_only {
            q = q.filter(st::enabled.eq(true));
        }
        q.order(st::ticker.asc())
            .load::<TokenRow>(&mut conn)
            .context("listing tokens")
    }

    /// One token by mint (byte-exact), or `None`.
    pub fn get(&self, mint: &str) -> Result<Option<TokenRow>> {
        let mut conn = self.pool.get().context("checkout conn")?;
        st::table
            .find(mint)
            .first::<TokenRow>(&mut conn)
            .optional()
            .context("getting token")
    }

    /// Insert or update a token. On conflict on `mint`, every non-key field
    /// is overwritten and `updated_at` is bumped.
    pub fn upsert(&self, t: UpsertToken) -> Result<TokenRow> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::insert_into(st::table)
            .values(&t)
            .on_conflict(st::mint)
            .do_update()
            .set((
                st::ticker.eq(excluded(st::ticker)),
                st::name.eq(excluded(st::name)),
                st::logo_uri.eq(excluded(st::logo_uri)),
                st::decimals.eq(excluded(st::decimals)),
                st::pyth_feed_id.eq(excluded(st::pyth_feed_id)),
                st::enabled.eq(excluded(st::enabled)),
                st::updated_at.eq(diesel::dsl::now),
            ))
            .get_result::<TokenRow>(&mut conn)
            .context("upserting token")
    }

    /// Delete a token by mint. Returns the number of rows removed (0 if it
    /// wasn't present).
    pub fn delete(&self, mint: &str) -> Result<usize> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::delete(st::table.find(mint))
            .execute(&mut conn)
            .context("deleting token")
    }
}
