//! Warn-level operational alerts (multichain plan §8):
//!
//! - `vault-messenger-queue-stalled` — the oldest undelivered message is
//!   older than the threshold.
//! - `vault-messenger-payout-queue-aged` — a hub-booked payable
//!   (SpokeWithdrawProcessed) is outstanding beyond the threshold with no
//!   PayoutReceipt.
//! - `vault-messenger-fee-pot-low` — the spoke fee pot (from
//!   SpokeStateSynced) is below the floor, well before exhaustion.
//!
//! Threshold logic is pure and unit-tested; the task wires it to the DB.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::db::repo::blocking;
use crate::state::AppState;
use crate::watcher::run_loop;

// ── pure threshold checks ──────────────────────────────────────────────

/// Age in seconds of `oldest`, when it breaches `threshold_secs`.
pub fn stalled_age_secs(
    oldest: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    threshold_secs: i64,
) -> Option<i64> {
    let age = (now - oldest?).num_seconds();
    (age > threshold_secs).then_some(age)
}

/// Fee pot below the floor? (Pot is a NUMERIC out of the DB.)
pub fn fee_pot_low(pot: &BigDecimal, floor_wei: u128) -> bool {
    *pot < BigDecimal::from_str(&floor_wei.to_string()).expect("u128 is a valid decimal")
}

// ── the task ───────────────────────────────────────────────────────────

pub struct AlertParams {
    pub state: Arc<AppState>,
    pub interval: Duration,
    pub queue_stalled_after_secs: i64,
    pub payout_aged_after_secs: i64,
    pub fee_pot_low_wei: u128,
}

pub fn spawn(p: AlertParams) {
    tokio::spawn(async move { run_loop("alerts", p.interval, || tick(&p)).await });
}

async fn tick(p: &AlertParams) -> Result<()> {
    let now = Utc::now();

    let oldest = blocking(&p.state.repo, |r| r.oldest_undelivered_created()).await?;
    if let Some(age) = stalled_age_secs(oldest, now, p.queue_stalled_after_secs) {
        warn!(
            alert_id = "vault-messenger-queue-stalled",
            oldest_pending_age_secs = age,
            threshold_secs = p.queue_stalled_after_secs,
            "message queue is stalled — oldest undelivered message is aging"
        );
    }

    let payable = blocking(&p.state.repo, |r| r.oldest_unsettled_payable()).await?;
    if let Some(row) = payable {
        if let Some(age) = stalled_age_secs(Some(row.created_at), now, p.payout_aged_after_secs) {
            warn!(
                alert_id = "vault-messenger-payout-queue-aged",
                spoke_id = row.spoke_id,
                request_seq = row.request_seq,
                pay_units = %row.pay_units,
                age_secs = age,
                threshold_secs = p.payout_aged_after_secs,
                "spoke payout outstanding beyond threshold without a receipt"
            );
        }
    }

    for stats in blocking(&p.state.repo, |r| r.lane_stats()).await? {
        if fee_pot_low(&stats.fee_pot, p.fee_pot_low_wei) {
            warn!(
                alert_id = "vault-messenger-fee-pot-low",
                spoke_id = stats.spoke_id,
                fee_pot = %stats.fee_pot,
                floor_wei = %p.fee_pot_low_wei,
                "spoke fee pot below the floor — top up before user sends start reverting"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as D;

    #[test]
    fn queue_stall_triggers_past_the_threshold_only() {
        let now = Utc::now();
        assert_eq!(stalled_age_secs(None, now, 900), None); // empty queue
        assert_eq!(stalled_age_secs(Some(now - D::seconds(899)), now, 900), None);
        assert_eq!(stalled_age_secs(Some(now - D::seconds(901)), now, 900), Some(901));
    }

    #[test]
    fn payout_aging_uses_the_same_gate() {
        let now = Utc::now();
        assert_eq!(stalled_age_secs(Some(now - D::seconds(3599)), now, 3600), None);
        assert!(stalled_age_secs(Some(now - D::seconds(7200)), now, 3600).is_some());
    }

    #[test]
    fn fee_pot_floor_comparison() {
        let floor = 100_000_000_000_000_000u128; // 0.1 native
        assert!(fee_pot_low(&BigDecimal::from_str("99999999999999999").unwrap(), floor));
        assert!(!fee_pot_low(&BigDecimal::from_str("100000000000000000").unwrap(), floor));
        // Values past u64 (the wire carries u128 fee pots) still compare.
        assert!(!fee_pot_low(
            &BigDecimal::from_str("777000000000000000000").unwrap(),
            floor
        ));
        assert!(fee_pot_low(&BigDecimal::from(0u32), floor));
    }
}
