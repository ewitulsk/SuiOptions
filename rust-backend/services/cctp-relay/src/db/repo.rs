//! Repository over the cctp-relay DB.

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};

use super::models::{status, NewTransfer, TransferRow};
use super::schema::cctp_transfers::dsl as t;
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: std::sync::Arc<DbPool>,
}

impl Repo {
    pub fn new(pool: std::sync::Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out DB connection")
    }

    /// Insert a new transfer; idempotent on (origin_chain, origin_tx_hash).
    /// Returns the (existing or new) row.
    pub fn insert_transfer(&self, new: NewTransfer) -> Result<TransferRow> {
        let mut conn = self.conn()?;
        diesel::insert_into(t::cctp_transfers)
            .values(&new)
            .on_conflict((t::origin_chain, t::origin_tx_hash))
            .do_nothing()
            .execute(&mut conn)
            .context("inserting transfer")?;
        t::cctp_transfers
            .filter(t::origin_chain.eq(&new.origin_chain))
            .filter(t::origin_tx_hash.eq(&new.origin_tx_hash))
            .first(&mut conn)
            .context("reading back inserted transfer")
    }

    /// Transfers for the bridge page, newest first.
    pub fn transfers_for_wallet(&self, wallet: &str, open_only: bool) -> Result<Vec<TransferRow>> {
        let mut conn = self.conn()?;
        let mut q = t::cctp_transfers
            .filter(t::origin_wallet.eq(wallet).or(t::destination_wallet.eq(wallet)))
            .order(t::created_at.desc())
            .limit(200)
            .into_boxed();
        if open_only {
            q = q.filter(t::status.ne(status::COMPLETE).and(t::status.ne(status::FAILED)));
        }
        q.load(&mut conn).context("listing transfers")
    }

    pub fn transfers_with_status(&self, s: &str) -> Result<Vec<TransferRow>> {
        let mut conn = self.conn()?;
        t::cctp_transfers
            .filter(t::status.eq(s))
            .order(t::created_at.asc())
            .load(&mut conn)
            .context("listing transfers by status")
    }

    /// Poller: attestation arrived — store message/attestation + decoded
    /// fields and advance to `attested`.
    pub fn mark_attested(
        &self,
        id: i64,
        message_hex: &str,
        attestation_hex: &str,
        amount: BigDecimal,
        mint_recipient: &str,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((
                t::status.eq(status::ATTESTED),
                t::message_hex.eq(message_hex),
                t::attestation_hex.eq(attestation_hex),
                t::amount.eq(amount),
                t::mint_recipient.eq(mint_recipient),
                t::attested_at.eq(diesel::dsl::now),
                t::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking transfer attested")?;
        Ok(())
    }

    pub fn set_burned_at(&self, id: i64, at: DateTime<Utc>) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((t::burned_at.eq(at), t::updated_at.eq(diesel::dsl::now)))
            .execute(&mut conn)
            .context("setting burned_at")?;
        Ok(())
    }

    /// Relayer: mint submitted, awaiting confirmation.
    pub fn mark_minting(&self, id: i64, mint_tx_hash: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((
                t::status.eq(status::MINTING),
                t::mint_tx_hash.eq(mint_tx_hash),
                t::attempts.eq(t::attempts + 1),
                t::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking transfer minting")?;
        Ok(())
    }

    /// Mint confirmed on the destination chain.
    pub fn mark_complete(&self, id: i64, minted_at: DateTime<Utc>, note: Option<&str>) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((
                t::status.eq(status::COMPLETE),
                t::minted_at.eq(minted_at),
                t::error.eq(note),
                t::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking transfer complete")?;
        Ok(())
    }

    /// Mint attempt failed: bump attempts, keep `attested` for retry.
    pub fn record_mint_failure(&self, id: i64, error: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((
                t::status.eq(status::ATTESTED),
                t::attempts.eq(t::attempts + 1),
                t::error.eq(error),
                t::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("recording mint failure")?;
        Ok(())
    }

    /// Terminal failure (max attempts exhausted / unrecoverable).
    pub fn mark_failed(&self, id: i64, error: &str) -> Result<()> {
        let mut conn = self.conn()?;
        diesel::update(t::cctp_transfers.find(id))
            .set((
                t::status.eq(status::FAILED),
                t::error.eq(error),
                t::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .context("marking transfer failed")?;
        Ok(())
    }
}
