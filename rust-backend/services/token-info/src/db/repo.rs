//! Postgres-facing repository for the durable supported-token catalog.
//!
//! This is the operator-managed catalog only. On dev/staging the `/tokens`
//! endpoint additionally overlays test tokens at read time (see `overlay.rs`)
//! — those never touch this table.

use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::upsert::excluded;

use super::models::{OrgRow, TokenRow, UpsertOrg, UpsertToken};
use super::schema::supported_tokens as st;
use super::schema::verified_orgs as vo;
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

    /// One token by coin type, or `None`.
    pub fn get(&self, coin_type: &str) -> Result<Option<TokenRow>> {
        let mut conn = self.pool.get().context("checkout conn")?;
        st::table
            .find(coin_type)
            .first::<TokenRow>(&mut conn)
            .optional()
            .context("getting token")
    }

    /// Insert or update a token (internal API). On conflict on `coin_type`,
    /// every non-key field is overwritten and `updated_at` is bumped.
    pub fn upsert(&self, t: UpsertToken) -> Result<TokenRow> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::insert_into(st::table)
            .values(&t)
            .on_conflict(st::coin_type)
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

    /// Delete a token by coin type. Returns the number of rows removed (0 if
    /// it wasn't present).
    pub fn delete(&self, coin_type: &str) -> Result<usize> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::delete(st::table.find(coin_type))
            .execute(&mut conn)
            .context("deleting token")
    }

    // ----------------------------------------------------- verified orgs

    /// All allowlist rows, name-sorted. `enabled_only` filters to enabled
    /// ("verified" = present AND enabled).
    pub fn list_orgs(&self, enabled_only: bool) -> Result<Vec<OrgRow>> {
        let mut conn = self.pool.get().context("checkout conn")?;
        let mut q = vo::table.into_boxed();
        if enabled_only {
            q = q.filter(vo::enabled.eq(true));
        }
        q.order(vo::name.asc())
            .load::<OrgRow>(&mut conn)
            .context("listing verified orgs")
    }

    /// One allowlist row by org id, or `None`.
    pub fn get_org(&self, org_id: &str) -> Result<Option<OrgRow>> {
        let mut conn = self.pool.get().context("checkout conn")?;
        vo::table
            .find(org_id)
            .first::<OrgRow>(&mut conn)
            .optional()
            .context("getting verified org")
    }

    /// Insert or update an allowlist row. On conflict on `org_id`, every
    /// non-key field is overwritten and `updated_at` is bumped.
    pub fn upsert_org(&self, o: UpsertOrg) -> Result<OrgRow> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::insert_into(vo::table)
            .values(&o)
            .on_conflict(vo::org_id)
            .do_update()
            .set((
                vo::name.eq(excluded(vo::name)),
                vo::enabled.eq(excluded(vo::enabled)),
                vo::updated_at.eq(diesel::dsl::now),
            ))
            .get_result::<OrgRow>(&mut conn)
            .context("upserting verified org")
    }

    /// Delete an allowlist row. Returns the number of rows removed.
    pub fn delete_org(&self, org_id: &str) -> Result<usize> {
        let mut conn = self.pool.get().context("checkout conn")?;
        diesel::delete(vo::table.find(org_id))
            .execute(&mut conn)
            .context("deleting verified org")
    }
}
