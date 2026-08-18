//! Postgres persistence for the orderbook service (spec §5.1 `store`),
//! diesel + r2d2 following the indexer's layout. Write-ahead discipline: an
//! order is persisted (`OPEN`) before it enters the in-memory book, so on
//! crash the book is rebuilt from `OPEN` orders and no acknowledged order
//! is ever lost (§5.4).
//!
//! [`Db`] is the async facade the axum handlers and sync/settlement tasks
//! use: every call checks out a pooled connection on the blocking pool.

pub mod models;
pub mod schema;

mod repo;

use std::sync::Arc;

use anyhow::{Context, Result};
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use exchange_types::{Digest, Market, Side, SignedOrder, SuiAddress};

pub use models::{FillRow, NewFill, VaultManagerRow};
pub use repo::{OrderStatus, Repo, StoredOrder};

pub type DbPool = Pool<ConnectionManager<PgConnection>>;
pub type ArcPool = Arc<DbPool>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Diesel(#[from] diesel::result::Error),
    #[error("connection pool: {0}")]
    Pool(String),
    #[error("amount {0} exceeds storable range")]
    AmountRange(u64),
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error("blocking task join: {0}")]
    Join(String),
}

impl StoreError {
    pub fn is_unique_violation(&self) -> bool {
        matches!(
            self,
            StoreError::Diesel(diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                _,
            ))
        )
    }
}

/// Embedded so the binary carries its own migration set; no separate
/// `diesel migration run` step required at boot.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/db/migrations");

pub fn establish_pool(database_url: &str, max_size: u32) -> Result<DbPool> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .with_context(|| "building r2d2 pool".to_string())
}

pub fn run_migrations(pool: &DbPool) -> Result<()> {
    let mut conn = pool.get().context("checking out connection for migrations")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("running migrations: {e}"))?;
    Ok(())
}

/// Async facade over [`Repo`]: each method hops to the blocking pool. Cheap
/// to clone; handlers hold it in `AppState`.
#[derive(Clone)]
pub struct Db {
    pool: ArcPool,
}

macro_rules! blocking {
    ($self:ident, |$repo:ident| $body:expr) => {{
        let pool = $self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let $repo = Repo::new(pool);
            $body
        })
        .await
        .map_err(|e| StoreError::Join(e.to_string()))?
    }};
}

impl Db {
    pub fn new(pool: ArcPool) -> Self {
        Db { pool }
    }

    pub async fn upsert_market(&self, m: &Market) -> Result<(), StoreError> {
        let m = m.clone();
        blocking!(self, |r| r.upsert_market(&m))
    }

    pub async fn disable_markets_absent_from(
        &self,
        current_ids: Vec<String>,
    ) -> Result<usize, StoreError> {
        blocking!(self, |r| r.disable_markets_absent_from(&current_ids))
    }

    pub async fn enabled_market_ids(&self) -> Result<Vec<String>, StoreError> {
        blocking!(self, |r| r.enabled_market_ids())
    }

    pub async fn insert_discovered_market(&self, m: &Market) -> Result<bool, StoreError> {
        let m = m.clone();
        blocking!(self, |r| r.insert_discovered_market(&m))
    }

    pub async fn enabled_markets(&self) -> Result<Vec<(Market, bool)>, StoreError> {
        blocking!(self, |r| r.enabled_markets())
    }

    pub async fn get_enabled_market(
        &self,
        registry_id: &str,
    ) -> Result<Option<(Market, bool)>, StoreError> {
        let registry_id = registry_id.to_owned();
        blocking!(self, |r| r.get_enabled_market(&registry_id))
    }

    pub async fn set_market_paused(
        &self,
        registry_id: &str,
        paused: bool,
    ) -> Result<(), StoreError> {
        let registry_id = registry_id.to_owned();
        blocking!(self, |r| r.set_market_paused(&registry_id, paused))
    }

    pub async fn set_market_fee(
        &self,
        registry_id: &str,
        fee_bps: u64,
    ) -> Result<(), StoreError> {
        let registry_id = registry_id.to_owned();
        blocking!(self, |r| r.set_market_fee(&registry_id, fee_bps))
    }

    pub async fn insert_order(
        &self,
        digest: &Digest,
        signed: &SignedOrder,
        side: Side,
        price_ticks: u64,
        order_bytes: &[u8],
    ) -> Result<(), StoreError> {
        let (digest, signed, bytes) = (*digest, signed.clone(), order_bytes.to_vec());
        blocking!(self, |r| r.insert_order(&digest, &signed, side, price_ticks, &bytes))
    }

    pub async fn get_order(&self, digest: &Digest) -> Result<Option<StoredOrder>, StoreError> {
        let digest = *digest;
        blocking!(self, |r| r.get_order(&digest))
    }

    pub async fn open_orders(
        &self,
        registry_id: &SuiAddress,
    ) -> Result<Vec<StoredOrder>, StoreError> {
        let id = *registry_id;
        blocking!(self, |r| r.open_orders(&id))
    }

    pub async fn open_orders_by_manager(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Vec<StoredOrder>, StoreError> {
        let id = *manager_id;
        blocking!(self, |r| r.open_orders_by_manager(&id))
    }

    pub async fn set_order_status(
        &self,
        digest: &Digest,
        status: OrderStatus,
    ) -> Result<(), StoreError> {
        let digest = *digest;
        blocking!(self, |r| r.set_order_status(&digest, status))
    }

    pub async fn max_salt(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<Option<u64>, StoreError> {
        let (reg, maker) = (*registry_id, *maker);
        blocking!(self, |r| r.max_salt(&reg, &maker))
    }

    pub async fn open_commitment(
        &self,
        manager_id: &SuiAddress,
        maker_token: &str,
    ) -> Result<u64, StoreError> {
        let (id, token) = (*manager_id, maker_token.to_owned());
        blocking!(self, |r| r.open_commitment(&id, &token))
    }

    pub async fn orders_by_account(
        &self,
        maker: &SuiAddress,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let maker = *maker;
        blocking!(self, |r| r.orders_by_account(&maker))
    }

    pub async fn apply_fill(&self, fill: NewFill) -> Result<bool, StoreError> {
        blocking!(self, |r| r.apply_fill(&fill))
    }

    pub async fn recent_trades(
        &self,
        registry_id: &SuiAddress,
        limit: i64,
    ) -> Result<Vec<FillRow>, StoreError> {
        let id = *registry_id;
        blocking!(self, |r| r.recent_trades(&id, limit))
    }

    pub async fn fills_by_account(&self, addr: &SuiAddress) -> Result<Vec<FillRow>, StoreError> {
        let addr = *addr;
        blocking!(self, |r| r.fills_by_account(&addr))
    }

    pub async fn apply_balance_delta(
        &self,
        manager_id: &SuiAddress,
        token: &str,
        delta: i64,
    ) -> Result<i64, StoreError> {
        let (id, token) = (*manager_id, token.to_owned());
        blocking!(self, |r| r.apply_balance_delta(&id, &token, delta))
    }

    pub async fn balance(&self, manager_id: &SuiAddress, token: &str) -> Result<u64, StoreError> {
        let (id, token) = (*manager_id, token.to_owned());
        blocking!(self, |r| r.balance(&id, &token))
    }

    pub async fn balances_of(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let id = *manager_id;
        blocking!(self, |r| r.balances_of(&id))
    }

    pub async fn set_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
        approved: bool,
    ) -> Result<(), StoreError> {
        let (id, signer) = (*manager_id, *signer);
        blocking!(self, |r| r.set_signer(&id, &signer, approved))
    }

    pub async fn is_approved_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
    ) -> Result<bool, StoreError> {
        let (id, signer) = (*manager_id, *signer);
        blocking!(self, |r| r.is_approved_signer(&id, &signer))
    }

    pub async fn set_watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
        min_valid_salt: u64,
    ) -> Result<(), StoreError> {
        let (reg, maker) = (*registry_id, *maker);
        blocking!(self, |r| r.set_watermark(&reg, &maker, min_valid_salt))
    }

    pub async fn watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<u64, StoreError> {
        let (reg, maker) = (*registry_id, *maker);
        blocking!(self, |r| r.watermark(&reg, &maker))
    }

    pub async fn upsert_vault_manager(&self, row: VaultManagerRow) -> Result<(), StoreError> {
        blocking!(self, |r| r.upsert_vault_manager(&row))
    }

    pub async fn vault_manager(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Option<VaultManagerRow>, StoreError> {
        let id = *manager_id;
        blocking!(self, |r| r.vault_manager(&id))
    }

    pub async fn save_cursor(&self, name: &str, cursor: &str) -> Result<(), StoreError> {
        let (name, cursor) = (name.to_owned(), cursor.to_owned());
        blocking!(self, |r| r.save_cursor(&name, &cursor))
    }

    pub async fn load_cursor(&self, name: &str) -> Result<Option<String>, StoreError> {
        let name = name.to_owned();
        blocking!(self, |r| r.load_cursor(&name))
    }
}
