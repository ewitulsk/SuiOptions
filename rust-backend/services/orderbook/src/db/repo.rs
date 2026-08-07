//! Synchronous diesel repository over the exchange schema. The async
//! surface the handlers use lives in [`super::Db`], which runs these on the
//! blocking pool.

use super::models::{FillRow, NewFill, NewMarket, NewOrder, OrderRow};
use super::schema::{
    exchange_approved_signers, exchange_balances, exchange_cursors, exchange_fills,
    exchange_markets, exchange_orders, exchange_salt_watermarks,
};
use super::{ArcPool, StoreError};
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::PgConnection;
use exchange_types::{Digest, Market, Side, SignedOrder, SuiAddress};

/// Order lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

fn to_i64(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::AmountRange(v))
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

fn stored_order(row: OrderRow) -> Result<StoredOrder, StoreError> {
    let signed: SignedOrder = serde_json::from_value(row.order_json)
        .map_err(|e| StoreError::Corrupt(e.to_string()))?;
    Ok(StoredOrder {
        digest: Digest::parse(&row.digest).map_err(|e| StoreError::Corrupt(e.to_string()))?,
        signed,
        side: if row.side == "bid" { Side::Bid } else { Side::Ask },
        price_ticks: row.price_ticks as u64,
        filled_taker: row.filled_taker as u64,
        status: row.status,
    })
}

const ORDER_COLUMNS: (
    exchange_orders::columns::digest,
    exchange_orders::columns::side,
    exchange_orders::columns::price_ticks,
    exchange_orders::columns::filled_taker,
    exchange_orders::columns::status,
    exchange_orders::columns::order_json,
) = (
    exchange_orders::digest,
    exchange_orders::side,
    exchange_orders::price_ticks,
    exchange_orders::filled_taker,
    exchange_orders::status,
    exchange_orders::order_json,
);

pub struct Repo {
    pool: ArcPool,
}

impl Repo {
    pub fn new(pool: ArcPool) -> Self {
        Repo { pool }
    }

    fn conn(
        &self,
    ) -> Result<PooledConnection<ConnectionManager<PgConnection>>, StoreError> {
        self.pool.get().map_err(|e| StoreError::Pool(e.to_string()))
    }

    // === Markets ===

    pub fn upsert_market(&self, m: &Market) -> Result<(), StoreError> {
        let row = NewMarket {
            registry_id: m.registry_id.to_hex(),
            symbol: m.symbol.clone(),
            base: m.base.clone(),
            quote: m.quote.clone(),
            tick_size: to_i64(m.tick_size)?,
            min_size: to_i64(m.min_size)?,
            lot_size: to_i64(m.lot_size)?,
            current_fee_bps: to_i64(m.current_fee_bps)?,
        };
        diesel::insert_into(exchange_markets::table)
            .values(&row)
            .on_conflict(exchange_markets::registry_id)
            .do_update()
            .set(&row)
            .execute(&mut self.conn()?)?;
        Ok(())
    }

    // === Orders ===

    pub fn insert_order(
        &self,
        digest: &Digest,
        signed: &SignedOrder,
        side: Side,
        price_ticks: u64,
        order_bytes: &[u8],
    ) -> Result<(), StoreError> {
        let o = &signed.order;
        let row = NewOrder {
            digest: digest.to_hex(),
            registry_id: signed.registry_id.to_hex(),
            maker: o.maker.to_hex(),
            manager_id: o.maker_manager_id.to_hex(),
            maker_token: o.maker_token.clone(),
            side: side_str(side).to_owned(),
            price_ticks: to_i64(price_ticks)?,
            salt: to_i64(o.salt)?,
            expiry_ms: to_i64(o.expiry_ms)?,
            taker_amount: to_i64(o.taker_amount)?,
            maker_amount: to_i64(o.maker_amount)?,
            order_json: serde_json::to_value(signed).expect("SignedOrder serializes"),
            order_bytes: order_bytes.to_vec(),
        };
        diesel::insert_into(exchange_orders::table)
            .values(&row)
            .execute(&mut self.conn()?)?;
        Ok(())
    }

    pub fn get_order(&self, digest: &Digest) -> Result<Option<StoredOrder>, StoreError> {
        let row: Option<OrderRow> = exchange_orders::table
            .filter(exchange_orders::digest.eq(digest.to_hex()))
            .select(ORDER_COLUMNS)
            .first(&mut self.conn()?)
            .optional()?;
        row.map(stored_order).transpose()
    }

    /// All OPEN orders of a market (book rebuild on restart), oldest first.
    pub fn open_orders(&self, registry_id: &SuiAddress) -> Result<Vec<StoredOrder>, StoreError> {
        let rows: Vec<OrderRow> = exchange_orders::table
            .filter(exchange_orders::registry_id.eq(registry_id.to_hex()))
            .filter(exchange_orders::status.eq("OPEN"))
            .order(exchange_orders::created_at.asc())
            .select(ORDER_COLUMNS)
            .load(&mut self.conn()?)?;
        rows.into_iter().map(stored_order).collect()
    }

    /// OPEN orders whose escrow lives in one manager (event-driven pruning).
    pub fn open_orders_by_manager(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Vec<StoredOrder>, StoreError> {
        let rows: Vec<OrderRow> = exchange_orders::table
            .filter(exchange_orders::manager_id.eq(manager_id.to_hex()))
            .filter(exchange_orders::status.eq("OPEN"))
            .order(exchange_orders::created_at.asc())
            .select(ORDER_COLUMNS)
            .load(&mut self.conn()?)?;
        rows.into_iter().map(stored_order).collect()
    }

    pub fn set_order_status(
        &self,
        digest: &Digest,
        status: OrderStatus,
    ) -> Result<(), StoreError> {
        diesel::update(
            exchange_orders::table.filter(exchange_orders::digest.eq(digest.to_hex())),
        )
        .set((
            exchange_orders::status.eq(status.as_str()),
            exchange_orders::updated_at.eq(diesel::dsl::now),
        ))
        .execute(&mut self.conn()?)?;
        Ok(())
    }

    /// Highest salt this maker has used on this market (intake monotonicity).
    pub fn max_salt(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<Option<u64>, StoreError> {
        let max: Option<i64> = exchange_orders::table
            .filter(exchange_orders::registry_id.eq(registry_id.to_hex()))
            .filter(exchange_orders::maker.eq(maker.to_hex()))
            .select(diesel::dsl::max(exchange_orders::salt))
            .first(&mut self.conn()?)?;
        Ok(max.map(|v| v as u64))
    }

    /// Sum of unfilled maker-token commitments of OPEN orders for a manager
    /// and token (the uncommitted-escrow check, §5.4 step 4). Per-order
    /// remaining is floored via u128, matching on-chain rounding.
    pub fn open_commitment(
        &self,
        manager_id: &SuiAddress,
        maker_token: &str,
    ) -> Result<u64, StoreError> {
        let rows: Vec<(i64, i64, i64)> = exchange_orders::table
            .filter(exchange_orders::manager_id.eq(manager_id.to_hex()))
            .filter(exchange_orders::maker_token.eq(maker_token))
            .filter(exchange_orders::status.eq("OPEN"))
            .select((
                exchange_orders::maker_amount,
                exchange_orders::taker_amount,
                exchange_orders::filled_taker,
            ))
            .load(&mut self.conn()?)?;
        let mut total: u64 = 0;
        for (maker_amount, taker_amount, filled) in rows {
            let (m, t, f) = (maker_amount as u64, taker_amount as u64, filled as u64);
            let paid_out = exchange_types::math::muldiv_floor(f, m, t.max(1));
            total = total.saturating_add(m.saturating_sub(paid_out));
        }
        Ok(total)
    }

    pub fn orders_by_account(
        &self,
        maker: &SuiAddress,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let rows: Vec<OrderRow> = exchange_orders::table
            .filter(exchange_orders::maker.eq(maker.to_hex()))
            .order(exchange_orders::created_at.desc())
            .limit(500)
            .select(ORDER_COLUMNS)
            .load(&mut self.conn()?)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "digest": r.digest,
                    "order": r.order_json,
                    "status": r.status,
                    "filledTaker": r.filled_taker.to_string(),
                })
            })
            .collect())
    }

    // === Fills ===

    /// Record a chain-confirmed fill; idempotent by event id. Also advances
    /// the order's cumulative fill and flips its status when complete.
    /// Returns whether the event was newly inserted.
    pub fn apply_fill(&self, fill: &NewFill) -> Result<bool, StoreError> {
        let mut conn = self.conn()?;
        conn.transaction::<bool, StoreError, _>(|conn| {
            let inserted = diesel::insert_into(exchange_fills::table)
                .values(fill)
                .on_conflict_do_nothing()
                .execute(conn)?
                > 0;
            if inserted {
                let row: Option<(i64, i64)> = exchange_orders::table
                    .filter(exchange_orders::digest.eq(&fill.digest))
                    .select((exchange_orders::filled_taker, exchange_orders::taker_amount))
                    .first(conn)
                    .optional()?;
                if let Some((filled, cap)) = row {
                    let new_filled = filled.max(fill.filled_total);
                    diesel::update(
                        exchange_orders::table
                            .filter(exchange_orders::digest.eq(&fill.digest)),
                    )
                    .set((
                        exchange_orders::filled_taker.eq(new_filled),
                        exchange_orders::updated_at.eq(diesel::dsl::now),
                    ))
                    .execute(conn)?;
                    if new_filled >= cap {
                        diesel::update(
                            exchange_orders::table
                                .filter(exchange_orders::digest.eq(&fill.digest)),
                        )
                        .set(exchange_orders::status.eq("FILLED"))
                        .execute(conn)?;
                    }
                }
            }
            Ok(inserted)
        })
    }

    pub fn recent_trades(
        &self,
        registry_id: &SuiAddress,
        limit: i64,
    ) -> Result<Vec<FillRow>, StoreError> {
        Ok(exchange_fills::table
            .filter(exchange_fills::registry_id.eq(registry_id.to_hex()))
            .order(exchange_fills::timestamp_ms.desc())
            .limit(limit)
            .load(&mut self.conn()?)?)
    }

    pub fn fills_by_account(&self, addr: &SuiAddress) -> Result<Vec<FillRow>, StoreError> {
        let hex = addr.to_hex();
        Ok(exchange_fills::table
            .filter(
                exchange_fills::maker
                    .eq(hex.clone())
                    .or(exchange_fills::taker.eq(hex)),
            )
            .order(exchange_fills::timestamp_ms.desc())
            .limit(500)
            .load(&mut self.conn()?)?)
    }

    // === Balances / signers / watermarks (chain mirrors, §5.7) ===

    /// Apply a signed delta, clamping at zero (streams from independent
    /// module cursors can arrive slightly out of order). Returns the new
    /// balance.
    pub fn apply_balance_delta(
        &self,
        manager_id: &SuiAddress,
        token: &str,
        delta: i64,
    ) -> Result<i64, StoreError> {
        let mut conn = self.conn()?;
        conn.transaction::<i64, StoreError, _>(|conn| {
            let existing: Option<i64> = exchange_balances::table
                .filter(exchange_balances::manager_id.eq(manager_id.to_hex()))
                .filter(exchange_balances::token.eq(token))
                .select(exchange_balances::amount)
                .first(conn)
                .optional()?;
            let new = existing.unwrap_or(0).saturating_add(delta).max(0);
            diesel::insert_into(exchange_balances::table)
                .values((
                    exchange_balances::manager_id.eq(manager_id.to_hex()),
                    exchange_balances::token.eq(token),
                    exchange_balances::amount.eq(new),
                ))
                .on_conflict((exchange_balances::manager_id, exchange_balances::token))
                .do_update()
                .set(exchange_balances::amount.eq(new))
                .execute(conn)?;
            Ok(new)
        })
    }

    pub fn balance(&self, manager_id: &SuiAddress, token: &str) -> Result<u64, StoreError> {
        let amount: Option<i64> = exchange_balances::table
            .filter(exchange_balances::manager_id.eq(manager_id.to_hex()))
            .filter(exchange_balances::token.eq(token))
            .select(exchange_balances::amount)
            .first(&mut self.conn()?)
            .optional()?;
        Ok(amount.unwrap_or(0).max(0) as u64)
    }

    pub fn balances_of(
        &self,
        manager_id: &SuiAddress,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(exchange_balances::table
            .filter(exchange_balances::manager_id.eq(manager_id.to_hex()))
            .select((exchange_balances::token, exchange_balances::amount))
            .load(&mut self.conn()?)?)
    }

    pub fn set_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
        approved: bool,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn()?;
        if approved {
            diesel::insert_into(exchange_approved_signers::table)
                .values((
                    exchange_approved_signers::manager_id.eq(manager_id.to_hex()),
                    exchange_approved_signers::signer.eq(signer.to_hex()),
                ))
                .on_conflict_do_nothing()
                .execute(&mut conn)?;
        } else {
            diesel::delete(
                exchange_approved_signers::table
                    .filter(exchange_approved_signers::manager_id.eq(manager_id.to_hex()))
                    .filter(exchange_approved_signers::signer.eq(signer.to_hex())),
            )
            .execute(&mut conn)?;
        }
        Ok(())
    }

    pub fn is_approved_signer(
        &self,
        manager_id: &SuiAddress,
        signer: &SuiAddress,
    ) -> Result<bool, StoreError> {
        let found: Option<String> = exchange_approved_signers::table
            .filter(exchange_approved_signers::manager_id.eq(manager_id.to_hex()))
            .filter(exchange_approved_signers::signer.eq(signer.to_hex()))
            .select(exchange_approved_signers::signer)
            .first(&mut self.conn()?)
            .optional()?;
        Ok(found.is_some())
    }

    /// Monotonic: keeps the max of the stored and supplied watermark.
    pub fn set_watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
        min_valid_salt: u64,
    ) -> Result<(), StoreError> {
        let salt = to_i64(min_valid_salt)?;
        let mut conn = self.conn()?;
        conn.transaction::<(), StoreError, _>(|conn| {
            let existing: Option<i64> = exchange_salt_watermarks::table
                .filter(exchange_salt_watermarks::registry_id.eq(registry_id.to_hex()))
                .filter(exchange_salt_watermarks::maker.eq(maker.to_hex()))
                .select(exchange_salt_watermarks::min_valid_salt)
                .first(conn)
                .optional()?;
            let new = existing.unwrap_or(0).max(salt);
            diesel::insert_into(exchange_salt_watermarks::table)
                .values((
                    exchange_salt_watermarks::registry_id.eq(registry_id.to_hex()),
                    exchange_salt_watermarks::maker.eq(maker.to_hex()),
                    exchange_salt_watermarks::min_valid_salt.eq(new),
                ))
                .on_conflict((
                    exchange_salt_watermarks::registry_id,
                    exchange_salt_watermarks::maker,
                ))
                .do_update()
                .set(exchange_salt_watermarks::min_valid_salt.eq(new))
                .execute(conn)?;
            Ok(())
        })
    }

    pub fn watermark(
        &self,
        registry_id: &SuiAddress,
        maker: &SuiAddress,
    ) -> Result<u64, StoreError> {
        let salt: Option<i64> = exchange_salt_watermarks::table
            .filter(exchange_salt_watermarks::registry_id.eq(registry_id.to_hex()))
            .filter(exchange_salt_watermarks::maker.eq(maker.to_hex()))
            .select(exchange_salt_watermarks::min_valid_salt)
            .first(&mut self.conn()?)
            .optional()?;
        Ok(salt.unwrap_or(0).max(0) as u64)
    }

    // === Cursors ===

    pub fn save_cursor(&self, name: &str, cursor: &str) -> Result<(), StoreError> {
        diesel::insert_into(exchange_cursors::table)
            .values((
                exchange_cursors::name.eq(name),
                exchange_cursors::cursor.eq(cursor),
            ))
            .on_conflict(exchange_cursors::name)
            .do_update()
            .set(exchange_cursors::cursor.eq(cursor))
            .execute(&mut self.conn()?)?;
        Ok(())
    }

    pub fn load_cursor(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(exchange_cursors::table
            .filter(exchange_cursors::name.eq(name))
            .select(exchange_cursors::cursor)
            .first(&mut self.conn()?)
            .optional()?)
    }
}
