//! Delivery loops: submit each lane's next-in-seq pending message and
//! confirm submitted ones, with capped exponential backoff. Terminal
//! failures fire `alert_id = "tx-failed-vault-messenger"` here (the
//! service handler), per docs/tx-alerting.md; benign already-applied
//! races (bad_sequence with the receiver's seq at/past ours) are
//! suppressed and confirmed instead.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, error, info, warn};
use vault_messages::MsgType;

use crate::db::models::{direction, status, MessageRow};
use crate::db::repo::blocking;
use crate::engine::{
    classify_failure, is_bad_sequence, order_gate, retry_due, FailureAction, OrderGate,
};
use crate::evm::SpokeChain;
use crate::hub::HubChain;
use crate::state::AppState;
use crate::watcher::run_loop;

pub struct DelivererParams {
    pub state: Arc<AppState>,
    pub hub: Arc<dyn HubChain>,
    pub spoke: Arc<dyn SpokeChain>,
    /// Submit hub→spoke deliveries ourselves (dev-relayer transport).
    /// LayerZero/CCIP lanes deliver themselves — we only confirm.
    pub submit_to_spoke: bool,
    pub max_attempts: i32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
    pub deliver_interval: Duration,
}

pub fn spawn(p: DelivererParams) {
    tokio::spawn(async move { run_loop("deliverer", p.deliver_interval, || tick(&p)).await });
}

async fn tick(p: &DelivererParams) -> Result<()> {
    deliver_spoke_to_hub(p).await?;
    confirm_hub_to_spoke(p).await?;
    deliver_hub_to_spoke(p).await?;
    Ok(())
}

/// Pending rows for one direction that are next-in-seq on their lane and
/// past their backoff. Stale duplicates (at/behind the confirmed
/// watermark) are confirmed inline.
async fn due_rows(p: &DelivererParams, dir: &'static str) -> Result<Vec<MessageRow>> {
    let repo = p.state.repo.clone();
    let rows = blocking(&repo, move |r| r.messages_with_status(dir, status::PENDING)).await?;
    let mut due = Vec::new();
    let now = Utc::now();
    for row in rows {
        let repo = p.state.repo.clone();
        let (d, s) = (row.direction.clone(), row.spoke_id);
        let last = blocking(&repo, move |r| r.last_confirmed_seq(&d, s)).await? as u64;
        match order_gate(last, row.seq as u64) {
            OrderGate::Deliver => {
                if retry_due(row.attempts, row.updated_at, now, p.backoff_base_secs, p.backoff_cap_secs) {
                    due.push(row);
                }
            }
            OrderGate::HoldBack => {
                debug!(lane = %row.direction, seq = row.seq, last, "held back — predecessor undelivered");
            }
            OrderGate::StaleDuplicate => {
                let repo = p.state.repo.clone();
                let id = row.id;
                blocking(&repo, move |r| {
                    r.mark_confirmed(id, None, Some("duplicate of an already-confirmed seq"))
                })
                .await?;
            }
        }
    }
    Ok(due)
}

// ── spoke → hub ────────────────────────────────────────────────────────

async fn deliver_spoke_to_hub(p: &DelivererParams) -> Result<()> {
    for row in due_rows(p, direction::SPOKE_TO_HUB).await? {
        let msg_type = match MsgType::from_u8(row.msg_type as u8) {
            Ok(t) => t,
            Err(e) => {
                fail_terminal(p, &row, &format!("unknown msg_type: {e}")).await;
                continue;
            }
        };
        let bytes = match hex::decode(&row.message_hex) {
            Ok(b) => b,
            Err(e) => {
                fail_terminal(p, &row, &format!("stored message not hex: {e}")).await;
                continue;
            }
        };
        match p.hub.deliver(msg_type, &bytes).await {
            Ok(digest) => {
                // submit_ptb waits for finality and asserts success, so
                // the hub leg confirms synchronously.
                let repo = p.state.repo.clone();
                let (id, d) = (row.id, digest.clone());
                blocking(&repo, move |r| r.mark_confirmed(id, Some(&d), None)).await?;
                info!(seq = row.seq, ?msg_type, %digest, "spoke->hub message delivered");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // bad_sequence may mean our own earlier attempt landed —
                // only the on-chain seq can say (multichain plan §2.1).
                let chain_seq = if is_bad_sequence(&msg) {
                    p.hub.spoke_inbound_seq().await.ok()
                } else {
                    None
                };
                resolve_failure(p, &row, chain_seq, &msg).await?;
            }
        }
    }
    Ok(())
}

// ── hub → spoke ────────────────────────────────────────────────────────

async fn confirm_hub_to_spoke(p: &DelivererParams) -> Result<()> {
    let repo = p.state.repo.clone();
    let rows = blocking(&repo, |r| {
        r.messages_with_status(direction::HUB_TO_SPOKE, status::SUBMITTED)
    })
    .await?;
    for row in rows {
        let Some(tx_hash) = row.tx_hash.clone() else {
            fail_terminal(p, &row, "submitted row missing tx_hash").await;
            continue;
        };
        match p.spoke.tx_status(&tx_hash).await? {
            Some(true) => {
                let repo = p.state.repo.clone();
                let id = row.id;
                blocking(&repo, move |r| r.mark_confirmed(id, None, None)).await?;
                info!(seq = row.seq, tx = %tx_hash, "hub->spoke delivery confirmed");
            }
            Some(false) => {
                // Reverted on-chain: maybe someone else delivered this seq.
                let chain_seq = p.spoke.last_inbound_seq().await.ok();
                resolve_failure(p, &row, chain_seq, "deliver tx reverted on chain").await?;
            }
            None => {
                // Unknown for >2 minutes → assume dropped and resubmit.
                if Utc::now() - row.updated_at > chrono::Duration::seconds(120) {
                    let repo = p.state.repo.clone();
                    let id = row.id;
                    blocking(&repo, move |r| {
                        r.record_failure(id, "deliver tx not found after 2m; resubmitting")
                    })
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn deliver_hub_to_spoke(p: &DelivererParams) -> Result<()> {
    let due = due_rows(p, direction::HUB_TO_SPOKE).await?;
    if due.is_empty() {
        return Ok(());
    }
    // One read serves the whole tick: self-delivering transports (and our
    // own landed txs) advance this.
    let applied = p.spoke.last_inbound_seq().await.unwrap_or(0);
    for row in due {
        if applied >= row.seq as u64 {
            let repo = p.state.repo.clone();
            let id = row.id;
            blocking(&repo, move |r| {
                r.mark_confirmed(id, None, Some("applied on the spoke (external delivery)"))
            })
            .await?;
            continue;
        }
        if !p.submit_to_spoke {
            debug!(seq = row.seq, "transport delivers itself; awaiting spoke seq advance");
            continue;
        }
        let bytes = match hex::decode(&row.message_hex) {
            Ok(b) => b,
            Err(e) => {
                fail_terminal(p, &row, &format!("stored message not hex: {e}")).await;
                continue;
            }
        };
        match p.spoke.deliver(&bytes).await {
            Ok(tx_hash) => {
                let repo = p.state.repo.clone();
                let (id, t) = (row.id, tx_hash.clone());
                blocking(&repo, move |r| r.mark_submitted(id, &t)).await?;
                info!(seq = row.seq, tx = %tx_hash, "hub->spoke delivery submitted");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let chain_seq = p.spoke.last_inbound_seq().await.ok();
                resolve_failure(p, &row, chain_seq, &msg).await?;
            }
        }
    }
    Ok(())
}

// ── failure handling ───────────────────────────────────────────────────

async fn resolve_failure(
    p: &DelivererParams,
    row: &MessageRow,
    chain_applied_seq: Option<u64>,
    msg: &str,
) -> Result<()> {
    match classify_failure(row.seq as u64, chain_applied_seq, row.attempts + 1, p.max_attempts) {
        FailureAction::AlreadyApplied => {
            info!(
                lane = %row.direction,
                seq = row.seq,
                "receiver already applied this seq — benign race; marking confirmed"
            );
            let repo = p.state.repo.clone();
            let id = row.id;
            blocking(&repo, move |r| {
                r.mark_confirmed(id, None, Some("already applied on-chain (bad_sequence race)"))
            })
            .await?;
        }
        FailureAction::Retry => {
            warn!(
                lane = %row.direction,
                seq = row.seq,
                attempts = row.attempts + 1,
                error = %msg,
                "delivery failed; will retry with backoff"
            );
            let repo = p.state.repo.clone();
            let (id, m) = (row.id, msg.to_string());
            blocking(&repo, move |r| r.record_failure(id, &m)).await?;
        }
        FailureAction::Terminal => fail_terminal(p, row, msg).await,
    }
    Ok(())
}

async fn fail_terminal(p: &DelivererParams, row: &MessageRow, msg: &str) {
    error!(
        alert_id = "tx-failed-vault-messenger",
        lane = %row.direction,
        spoke_id = row.spoke_id,
        seq = row.seq,
        msg_type = row.msg_type,
        attempts = row.attempts + 1,
        error = %msg,
        "message delivery failed terminally — the lane is blocked until resolved"
    );
    let repo = p.state.repo.clone();
    let (id, m) = (row.id, msg.to_string());
    let _ = blocking(&repo, move |r| r.mark_failed(id, &m))
        .await
        .map_err(|e| warn!(error = %format!("{e:#}"), "marking terminal failure"));
}

#[cfg(test)]
mod tests {
    //! Mock-chain classification flows: the deliverer's failure handling
    //! consults the mocked receiver seq exactly as the live one does.

    use super::*;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockHub {
        inbound_seq: u64,
        deliver_results: Mutex<Vec<Result<String>>>,
    }

    #[async_trait]
    impl HubChain for MockHub {
        async fn deliver(&self, _t: MsgType, _m: &[u8]) -> Result<String> {
            self.deliver_results
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Err(anyhow!("unexpected deliver")))
        }
        async fn send_config_sync(&self) -> Result<String> {
            Ok("digest".into())
        }
        async fn spoke_inbound_seq(&self) -> Result<u64> {
            Ok(self.inbound_seq)
        }
    }

    struct MockSpoke {
        applied: u64,
    }

    #[async_trait]
    impl crate::evm::SpokeChain for MockSpoke {
        async fn latest_block(&self) -> Result<u64> {
            Ok(0)
        }
        async fn outbound_logs(&self, _f: u64, _t: u64) -> Result<Vec<crate::evm::SpokeLog>> {
            Ok(vec![])
        }
        async fn deliver(&self, _m: &[u8]) -> Result<String> {
            Err(anyhow!("execution reverted: BadSeq"))
        }
        async fn sync_state(&self) -> Result<String> {
            Ok("0xtx".into())
        }
        async fn tx_status(&self, _h: &str) -> Result<Option<bool>> {
            Ok(Some(false))
        }
        async fn last_inbound_seq(&self) -> Result<u64> {
            Ok(self.applied)
        }
    }

    /// A hub abort with bad_sequence(143) while the hub's applied seq is
    /// at/past ours classifies as AlreadyApplied — the mock consults run
    /// through the same trait surface the live deliverer uses.
    #[tokio::test]
    async fn bad_sequence_race_resolves_via_the_hub_seq_recheck() {
        let hub = MockHub {
            inbound_seq: 5,
            deliver_results: Mutex::new(vec![Err(anyhow!(
                "handle_deposit_notice reverted: Failure {{ error: MoveAbort(MoveLocation \
                 {{ module: ModuleId {{ address: 0x1, name: Identifier(\"spoke\") }}, \
                 function: 3, instruction: 21 }}, 143) }}"
            ))]),
        };
        let err = hub.deliver(MsgType::DepositNotice, &[]).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(is_bad_sequence(&msg));
        let chain_seq = hub.spoke_inbound_seq().await.ok();
        assert_eq!(classify_failure(5, chain_seq, 1, 8), FailureAction::AlreadyApplied);
        // Seq 6 (not yet applied on-chain) keeps retrying instead.
        assert_eq!(classify_failure(6, chain_seq, 1, 8), FailureAction::Retry);
    }

    /// A reverted spoke deliver with the spoke's applied seq behind ours
    /// retries until the attempt budget, then goes terminal.
    #[tokio::test]
    async fn spoke_revert_classification() {
        let spoke = MockSpoke { applied: 3 };
        let err = SpokeChain::deliver(&spoke, &[]).await.unwrap_err();
        assert!(!is_bad_sequence(&format!("{err:#}")));
        let applied = spoke.last_inbound_seq().await.ok();
        assert_eq!(classify_failure(4, applied, 2, 8), FailureAction::Retry);
        assert_eq!(classify_failure(4, applied, 8, 8), FailureAction::Terminal);
        // Once the spoke reports the seq applied, the same failure is benign.
        assert_eq!(classify_failure(3, applied, 2, 8), FailureAction::AlreadyApplied);
    }
}
