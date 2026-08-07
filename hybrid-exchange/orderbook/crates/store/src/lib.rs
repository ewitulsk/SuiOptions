//! Postgres persistence (spec §5.1 `store`): orders, fills, balances,
//! cursors. Write-ahead discipline: an order is persisted (`OPEN`) before it
//! enters the in-memory book, so on crash the book is rebuilt from `OPEN`
//! orders and no acknowledged order is ever lost (§5.4).

use orderbook_core::{Digest, Market, Side, SignedOrder, SuiAddress};
use serde::Serialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("amount {0} exceeds storable range")]
    AmountRange(u64),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

impl StoreError {
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, StoreError::Sqlx(sqlx::Error::Database(db)) if db.is_unique_violation())
    }
}

fn to_i64(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::AmountRange(v))
}

/// Order lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Open,
    Filled,
    Cancelled,
    Pruned,
    Expired,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::Open => "OPEN",
            OrderStatus::Filled => "FILLED",
            OrderStatus::Cancelled => "CANCELLED",
            OrderStatus::Pruned => "PRUNED",
            OrderStatus::Expired => "EXPIRED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StoredOrder {
    pub digest: Digest,
    pub signed: SignedOrder,
    pub side: Side,
    pub price_ticks: u64,
    pub filled_taker: u64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillRow {
    pub digest: String,
    pub maker: String,
    pub taker: String,
    pub base_amount: i64,
    pub quote_amount: i64,
    pub maker_fee: i64,
    pub taker_fee: i64,
    pub maker_sold_base: bool,
    pub filled_total: i64,
    pub timestamp_ms: i64,
    pub tx_digest: String,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(database_url)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // === Markets ===

    pub async fn upsert_market(&self, m: &Market) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO markets (registry_id, symbol, base, quote, tick_size, min_size, lot_size, current_fee_bps)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT (registry_id) DO UPDATE SET
               symbol=$2, base=$3, quote=$4, tick_size=$5, min_size=$6, lot_size=$7, current_fee_bps=$8",
        )
        .bind(m.registry_id.to_hex())
        .bind(&m.symbol)
        .bind(&m.base)
        .bind(&m.quote)
        .bind(to_i64(m.tick_size)?)
        .bind(to_i64(m.min_size)?)
        .bind(to_i64(m.lot_size)?)
        .bind(to_i64(m.current_fee_bps)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // === Orders ===

    pub async fn insert_order(
        &self,
        digest: &Digest,
        signed: &SignedOrder,
        side: Side,
        price_ticks: u64,
        order_bytes: &[u8],
    ) -> Result<(), StoreError> {
        let o = &signed.order;
        sqlx::query(
            "INSERT INTO orders (digest, registry_id, maker, manager_id, side, price_ticks, salt,
                                 expiry_ms, taker_amount, maker_amount, order_json, order_bytes)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(digest.to_hex())
        .bind(signed.registry_id.to_hex())
        .bind(o.maker.to_hex())
        .bind(o.maker_manager_id.to_hex())
        .bind(match side {
            Side::Bid => "bid",
            Side::Ask => "ask",
        })
        .bind(to_i64(price_ticks)?)
        .bind(to_i64(o.salt)?)
        .bind(to_i64(o.expiry_ms)?)
        .bind(to_i64(o.taker_amount)?)
        .bind(to_i64(o.maker_amount)?)
        .bind(serde_json::to_value(signed).expect("SignedOrder serializes"))
        .bind(order_bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_order(&self, digest: &Digest) -> Result<Option<StoredOrder>, StoreError> {
        let row = sqlx::query(
            "SELECT order_json, side, price_ticks, filled_taker, status FROM orders WHERE digest = $1",
        )
        .bind(digest.to_hex())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let signed: SignedOrder = serde_json::from_value(r.get("order_json"))
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            Ok(StoredOrder {
                digest: *digest,
                signed,
                side: if r.get::<String, _>("side") == "bid" { Side::Bid } else { Side::Ask },
                price_ticks: r.get::<i64, _>("price_ticks") as u64,
                filled_taker: r.get::<i64, _>("filled_taker") as u64,
                status: r.get("status"),
            })
        })
        .transpose()
    }

    /// All OPEN orders of a market (book rebuild on restart).
    pub async fn open_orders(&self, registry_id: &SuiAddress) -> Result<Vec<StoredOrder>, StoreError> {
        let rows = sqlx::query(
            "SELECT digest, order_json, side, price_ticks, filled_taker, status
             FROM orders WHERE registry_id = $1 AND status = 'OPEN'
             ORDER BY created_at ASC",
        )
        .bind(registry_id.to_hex())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let signed: SignedOrder = serde_json::from_value(r.get("order_json"))
                    .map_err(|e| StoreError::Corrupt(e.to_string()))?;
                Ok(StoredOrder {
                    digest: Digest::parse(&r.get::<String, _>("digest"))
                        .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                    signed,
                    side: if r.get::<String, _>("side") == "bid" { Side::Bid } else { Side::Ask },
                    price_ticks: r.get::<i64, _>("price_ticks") as u64,
                    filled_taker: r.get::<i64, _>("filled_taker") as u64,
                    status: r.get("status"),
                })
            })
            .collect()
    }

    pub async fn set_order_status(
        &self,
        digest: &Digest,
        status: OrderStatus,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE digest = $1")
            .bind(digest.to_hex())
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Highest salt this maker has used on this market (intake monotonicity).
    pub async fn max_salt(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<Option<u64>, StoreError> {
        let row = sqlx::query(
            "SELECT MAX(salt) AS s FROM orders WHERE registry_id = $1 AND maker = $2",
        )
        .bind(registry_id.to_hex())
        .bind(maker.to_hex())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<Option<i64>, _>("s").map(|v| v as u64))
    }

    /// Sum of unfilled maker-token commitments of OPEN orders for a manager
    /// and token (the uncommitted-escrow check, §5.4 step 4).
    pub async fn open_commitment(
        &self,
        manager_id: &SuiAddress,
        maker_token: &str,
    ) -> Result<u64, StoreError> {
        // remaining maker out ≈ maker_amount * (1 - filled/taker) — floor'd
        // per order via integer math in SQL
        let row = sqlx::query(
            "SELECT COALESCE(SUM(maker_amount - (maker_amount * filled_taker) / taker_amount), 0)::BIGINT AS c
             FROM orders o
             WHERE manager_id = $1 AND status = 'OPEN'
               AND (order_json->>'makerToken') = $2",
        )
        .bind(manager_id.to_hex())
        .bind(maker_token)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("c").max(0) as u64)
    }

    pub async fn orders_by_account(
        &self,
        maker: &SuiAddress,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let rows = sqlx::query(
            "SELECT digest, order_json, status, filled_taker FROM orders
             WHERE maker = $1 ORDER BY created_at DESC LIMIT 500",
        )
        .bind(maker.to_hex())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "digest": r.get::<String, _>("digest"),
                    "order": r.get::<serde_json::Value, _>("order_json"),
                    "status": r.get::<String, _>("status"),
                    "filledTaker": r.get::<i64, _>("filled_taker").to_string(),
                })
            })
            .collect())
    }

    // === Fills ===

    /// Record a chain-confirmed fill; idempotent by event id. Also advances
    /// the order's cumulative fill and flips its status when complete.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_fill(
        &self,
        tx_digest: &str,
        event_seq: &str,
        digest: &Digest,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
        taker: &SuiAddress,
        base_amount: u64,
        quote_amount: u64,
        maker_fee: u64,
        taker_fee: u64,
        maker_sold_base: bool,
        filled_total: u64,
        timestamp_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO fills (tx_digest, event_seq, digest, registry_id, maker, taker,
                                base_amount, quote_amount, maker_fee, taker_fee,
                                maker_sold_base, filled_total, timestamp_ms)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
             ON CONFLICT DO NOTHING",
        )
        .bind(tx_digest)
        .bind(event_seq)
        .bind(digest.to_hex())
        .bind(registry_id.to_hex())
        .bind(maker.to_hex())
        .bind(taker.to_hex())
        .bind(to_i64(base_amount)?)
        .bind(to_i64(quote_amount)?)
        .bind(to_i64(maker_fee)?)
        .bind(to_i64(taker_fee)?)
        .bind(maker_sold_base)
        .bind(to_i64(filled_total)?)
        .bind(to_i64(timestamp_ms)?)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if inserted {
            sqlx::query(
                "UPDATE orders SET
                   filled_taker = GREATEST(filled_taker, $2),
                   status = CASE WHEN GREATEST(filled_taker, $2) >= taker_amount
                                 THEN 'FILLED' ELSE status END,
                   updated_at = now()
                 WHERE digest = $1",
            )
            .bind(digest.to_hex())
            .bind(to_i64(filled_total)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn recent_trades(
        &self,
        registry_id: &SuiAddress,
        limit: i64,
    ) -> Result<Vec<FillRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT digest, maker, taker, base_amount, quote_amount, maker_fee, taker_fee,
                    maker_sold_base, filled_total, timestamp_ms, tx_digest
             FROM fills WHERE registry_id = $1 ORDER BY timestamp_ms DESC LIMIT $2",
        )
        .bind(registry_id.to_hex())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(fill_row).collect())
    }

    pub async fn fills_by_account(
        &self,
        addr: &SuiAddress,
    ) -> Result<Vec<FillRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT digest, maker, taker, base_amount, quote_amount, maker_fee, taker_fee,
                    maker_sold_base, filled_total, timestamp_ms, tx_digest
             FROM fills WHERE maker = $1 OR taker = $1 ORDER BY timestamp_ms DESC LIMIT 500",
        )
        .bind(addr.to_hex())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(fill_row).collect())
    }

    // === Balances / signers / watermarks (chain mirrors, §5.7) ===

    pub async fn apply_balance_delta(
        &self,
        manager_id: &SuiAddress,
        token: &str,
        delta: i64,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query(
            "INSERT INTO balances (manager_id, token, amount) VALUES ($1,$2,GREATEST($3,0))
             ON CONFLICT (manager_id, token)
               DO UPDATE SET amount = GREATEST(balances.amount + $3, 0)
             RETURNING amount",
        )
        .bind(manager_id.to_hex())
        .bind(token)
        .bind(delta)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("amount"))
    }

    pub async fn balance(
        &self,
        manager_id: &SuiAddress,
        token: &str,
    ) -> Result<u64, StoreError> {
        let row = sqlx::query(
            "SELECT amount FROM balances WHERE manager_id = $1 AND token = $2",
        )
        .bind(manager_id.to_hex())
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<i64, _>("amount").max(0) as u64).unwrap_or(0))
    }

    pub async fn balances_of(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let rows =
            sqlx::query("SELECT token, amount FROM balances WHERE manager_id = $1")
                .bind(manager_id.to_hex())
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|r| (r.get("token"), r.get("amount"))).collect())
    }

    pub async fn set_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
        approved: bool,
    ) -> Result<(), StoreError> {
        if approved {
            sqlx::query(
                "INSERT INTO approved_signers (manager_id, signer) VALUES ($1,$2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(manager_id.to_hex())
            .bind(signer.to_hex())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("DELETE FROM approved_signers WHERE manager_id = $1 AND signer = $2")
                .bind(manager_id.to_hex())
                .bind(signer.to_hex())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn is_approved_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
    ) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT 1 AS one FROM approved_signers WHERE manager_id = $1 AND signer = $2",
        )
        .bind(manager_id.to_hex())
        .bind(signer.to_hex())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn set_watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
        min_valid_salt: u64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO salt_watermarks (registry_id, maker, min_valid_salt) VALUES ($1,$2,$3)
             ON CONFLICT (registry_id, maker)
               DO UPDATE SET min_valid_salt = GREATEST(salt_watermarks.min_valid_salt, $3)",
        )
        .bind(registry_id.to_hex())
        .bind(maker.to_hex())
        .bind(to_i64(min_valid_salt)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<u64, StoreError> {
        let row = sqlx::query(
            "SELECT min_valid_salt FROM salt_watermarks WHERE registry_id = $1 AND maker = $2",
        )
        .bind(registry_id.to_hex())
        .bind(maker.to_hex())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<i64, _>("min_valid_salt") as u64).unwrap_or(0))
    }

    // === Cursor ===

    pub async fn save_cursor(
        &self,
        name: &str,
        tx_digest: &str,
        event_seq: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO cursors (name, tx_digest, event_seq) VALUES ($1,$2,$3)
             ON CONFLICT (name) DO UPDATE SET tx_digest = $2, event_seq = $3",
        )
        .bind(name)
        .bind(tx_digest)
        .bind(event_seq)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_cursor(&self, name: &str) -> Result<Option<(String, String)>, StoreError> {
        let row = sqlx::query("SELECT tx_digest, event_seq FROM cursors WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get("tx_digest"), r.get("event_seq"))))
    }
}

fn fill_row(r: sqlx::postgres::PgRow) -> FillRow {
    FillRow {
        digest: r.get("digest"),
        maker: r.get("maker"),
        taker: r.get("taker"),
        base_amount: r.get("base_amount"),
        quote_amount: r.get("quote_amount"),
        maker_fee: r.get("maker_fee"),
        taker_fee: r.get("taker_fee"),
        maker_sold_base: r.get("maker_sold_base"),
        filled_total: r.get("filled_total"),
        timestamp_ms: r.get("timestamp_ms"),
        tx_digest: r.get("tx_digest"),
    }
}
