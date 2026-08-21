//! Read-model queries.

use std::sync::Arc;

use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::upsert::excluded;
use serde::Serialize;

use super::models::*;
use super::schema::{accounts, assets, customers, fee_schedule, ledger_events, wallets, webhook_errors};
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: Arc<DbPool>,
}

/// Per-customer rollup behind the flow-tracking views.
#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct CustomerFlow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub dakota_customer_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub customer_type: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub sub_client_id: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub asset: Option<String>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub events: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    pub inbound_minor: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    pub outbound_minor: Option<i64>,
}

impl Repo {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<diesel::r2d2::ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out a db connection")
    }

    // -------------------------------------------------------------- assets

    pub fn list_assets(&self) -> Result<Vec<Asset>> {
        let mut conn = self.conn()?;
        assets::table
            .order((assets::sort_order.asc(), assets::symbol.asc()))
            .select(Asset::as_select())
            .load(&mut conn)
            .context("listing assets")
    }

    /// Insert or update by `(symbol, network_id)` — the natural key, so the
    /// admin editing a row twice does not create a duplicate.
    pub fn upsert_asset(&self, a: &UpsertAsset) -> Result<Asset> {
        let mut conn = self.conn()?;
        diesel::insert_into(assets::table)
            .values(a)
            .on_conflict((assets::symbol, assets::network_id))
            .do_update()
            .set((
                assets::onramp_enabled.eq(excluded(assets::onramp_enabled)),
                assets::offramp_enabled.eq(excluded(assets::offramp_enabled)),
                assets::swap_enabled.eq(excluded(assets::swap_enabled)),
                assets::sort_order.eq(excluded(assets::sort_order)),
                assets::updated_at.eq(diesel::dsl::now),
            ))
            .returning(Asset::as_returning())
            .get_result(&mut conn)
            .context("upserting asset")
    }

    pub fn delete_asset(&self, id: i32) -> Result<usize> {
        let mut conn = self.conn()?;
        diesel::delete(assets::table.find(id))
            .execute(&mut conn)
            .context("deleting asset")
    }

    /// Whether `(symbol, network)` is enabled for `flow`.
    ///
    /// This is the allow-list every ramp handler checks before calling Dakota:
    /// the catalog is ours, so an asset we have not enabled must not be
    /// reachable just because a caller typed it into a request body.
    pub fn asset_allows(&self, symbol: &str, network_id: &str, flow: &str) -> Result<bool> {
        let mut conn = self.conn()?;
        let found: Option<Asset> = assets::table
            .filter(assets::symbol.eq(symbol))
            .filter(assets::network_id.eq(network_id))
            .select(Asset::as_select())
            .first(&mut conn)
            .optional()
            .context("checking asset")?;
        Ok(match (found, flow) {
            (Some(a), "onramp") => a.onramp_enabled,
            (Some(a), "offramp") => a.offramp_enabled,
            (Some(a), "swap") => a.swap_enabled,
            _ => false,
        })
    }

    // -------------------------------------------------------- fee schedule

    pub fn current_fees(&self) -> Result<Option<FeeSchedule>> {
        let mut conn = self.conn()?;
        fee_schedule::table
            .order(fee_schedule::effective_from.desc())
            .select(FeeSchedule::as_select())
            .first(&mut conn)
            .optional()
            .context("loading fee schedule")
    }

    /// Append a new schedule rather than mutating the old one, so a rate that
    /// applied to a past transaction stays recoverable.
    pub fn record_fees(&self, f: &NewFeeSchedule) -> Result<FeeSchedule> {
        let mut conn = self.conn()?;
        diesel::insert_into(fee_schedule::table)
            .values(f)
            .returning(FeeSchedule::as_returning())
            .get_result(&mut conn)
            .context("recording fee schedule")
    }

    // ----------------------------------------------------------- customers

    pub fn upsert_customer(&self, c: &UpsertCustomer) -> Result<Customer> {
        let mut conn = self.conn()?;
        diesel::insert_into(customers::table)
            .values(c)
            .on_conflict(customers::dakota_customer_id)
            .do_update()
            .set((
                customers::kyb_status.eq(excluded(customers::kyb_status)),
                customers::kyc_status.eq(excluded(customers::kyc_status)),
                customers::application_status.eq(excluded(customers::application_status)),
                customers::application_id.eq(excluded(customers::application_id)),
                customers::updated_at.eq(diesel::dsl::now),
            ))
            .returning(Customer::as_returning())
            .get_result(&mut conn)
            .context("upserting customer")
    }

    pub fn get_customer(&self, id: &str) -> Result<Option<Customer>> {
        let mut conn = self.conn()?;
        customers::table
            .find(id)
            .select(Customer::as_select())
            .first(&mut conn)
            .optional()
            .context("loading customer")
    }

    /// List customers, optionally narrowed to one sub-client's roster. The
    /// `sub_client` filter is how a business's session is confined to its own
    /// customers.
    pub fn list_customers(&self, sub_client: Option<&str>) -> Result<Vec<Customer>> {
        let mut conn = self.conn()?;
        let mut q = customers::table.into_boxed();
        if let Some(sub) = sub_client {
            q = q.filter(customers::sub_client_id.eq(sub.to_string()));
        }
        q.order(customers::created_at.desc())
            .select(Customer::as_select())
            .load(&mut conn)
            .context("listing customers")
    }

    pub fn list_sub_clients(&self) -> Result<Vec<Customer>> {
        let mut conn = self.conn()?;
        customers::table
            .filter(customers::is_sub_client.eq(true))
            .order(customers::created_at.desc())
            .select(Customer::as_select())
            .load(&mut conn)
            .context("listing sub-clients")
    }

    // ------------------------------------------------------------ accounts

    pub fn insert_account(&self, a: &NewAccount) -> Result<Account> {
        let mut conn = self.conn()?;
        diesel::insert_into(accounts::table)
            .values(a)
            .on_conflict(accounts::dakota_account_id)
            .do_nothing()
            .returning(Account::as_returning())
            .get_result(&mut conn)
            .context("inserting account")
    }

    pub fn list_accounts(&self, customer_id: Option<&str>) -> Result<Vec<Account>> {
        let mut conn = self.conn()?;
        let mut q = accounts::table.into_boxed();
        if let Some(c) = customer_id {
            q = q.filter(accounts::dakota_customer_id.eq(c.to_string()));
        }
        q.order(accounts::created_at.desc())
            .select(Account::as_select())
            .load(&mut conn)
            .context("listing accounts")
    }

    /// Which customer an account belongs to — the scope check for any
    /// account-addressed request.
    pub fn account_owner(&self, account_id: &str) -> Result<Option<String>> {
        let mut conn = self.conn()?;
        accounts::table
            .find(account_id)
            .select(accounts::dakota_customer_id)
            .first(&mut conn)
            .optional()
            .context("resolving account owner")
    }

    // -------------------------------------------------------------- ledger

    /// Record an observation. Idempotent on `event_id`: Dakota retries
    /// deliveries up to ~10 times over 48h, and a redelivery must not
    /// double-count a transfer.
    ///
    /// Returns true when the row was new.
    pub fn record_event(&self, e: &NewLedgerEvent) -> Result<bool> {
        let mut conn = self.conn()?;
        let inserted = diesel::insert_into(ledger_events::table)
            .values(e)
            .on_conflict(ledger_events::event_id)
            .do_nothing()
            .execute(&mut conn)
            .context("recording ledger event")?;
        Ok(inserted == 1)
    }

    pub fn list_events(&self, customer_id: Option<&str>, limit: i64) -> Result<Vec<LedgerEvent>> {
        let mut conn = self.conn()?;
        let mut q = ledger_events::table.into_boxed();
        if let Some(c) = customer_id {
            q = q.filter(ledger_events::dakota_customer_id.eq(c.to_string()));
        }
        q.order(ledger_events::occurred_at.desc().nulls_last())
            .limit(limit.clamp(1, 500))
            .select(LedgerEvent::as_select())
            .load(&mut conn)
            .context("listing ledger events")
    }

    /// Per-customer, per-asset flow totals.
    ///
    /// `sub_client` narrows to one partner's roster, which is what a business
    /// session sees; `None` is the platform-wide admin view.
    pub fn customer_flows(&self, sub_client: Option<&str>) -> Result<Vec<CustomerFlow>> {
        use diesel::sql_types::{Nullable, Text};
        let mut conn = self.conn()?;
        // Raw SQL: the conditional aggregates below have no clean diesel DSL
        // equivalent, and this is a reporting query rather than a hot path.
        diesel::sql_query(
            r#"
            SELECT c.dakota_customer_id,
                   c.customer_type,
                   c.sub_client_id,
                   e.asset,
                   COUNT(e.event_id)                                                   AS events,
                   -- ::bigint is load-bearing. Postgres widens SUM(bigint) to
                   -- NUMERIC, which does not match the BigInt this row binds to,
                   -- and the whole query fails at deserialization rather than in
                   -- the database.
                   SUM(CASE WHEN e.direction = 'in'  THEN e.amount_minor END)::bigint  AS inbound_minor,
                   SUM(CASE WHEN e.direction = 'out' THEN e.amount_minor END)::bigint  AS outbound_minor
            FROM customers c
            LEFT JOIN ledger_events e ON e.dakota_customer_id = c.dakota_customer_id
            WHERE ($1::text IS NULL OR c.sub_client_id = $1)
            GROUP BY c.dakota_customer_id, c.customer_type, c.sub_client_id, e.asset
            ORDER BY c.dakota_customer_id, e.asset
            "#,
        )
        .bind::<Nullable<Text>, _>(sub_client)
        .load::<CustomerFlow>(&mut conn)
        .context("aggregating customer flows")
    }

    // ------------------------------------------------------------- wallets

    pub fn insert_wallet(&self, w: &NewWallet) -> Result<Wallet> {
        let mut conn = self.conn()?;
        diesel::insert_into(wallets::table)
            .values(w)
            .returning(Wallet::as_returning())
            .get_result(&mut conn)
            .context("inserting wallet")
    }

    pub fn list_wallets(&self) -> Result<Vec<Wallet>> {
        let mut conn = self.conn()?;
        wallets::table
            .order(wallets::created_at.desc())
            .select(Wallet::as_select())
            .load(&mut conn)
            .context("listing wallets")
    }

    // ------------------------------------------------------ webhook errors

    pub fn record_webhook_error(&self, e: &NewWebhookError) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::insert_into(webhook_errors::table)
            .values(e)
            .execute(&mut conn)
            .context("recording webhook error")?;
        Ok(())
    }

    /// Cheap liveness probe that actually touches Postgres.
    pub fn ping(&self) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::sql_query("SELECT 1").execute(&mut conn).context("pinging db")?;
        Ok(())
    }
}
