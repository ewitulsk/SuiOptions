//! Pure lane/state-machine logic: seq-order gating, capped exponential
//! backoff, and submit-error classification. Everything here is
//! deterministic and network-free — the deliverer wires it to the chain
//! clients and the DB.

use chrono::{DateTime, Duration, Utc};
use vault_messages::MsgType;

/// The Move abort code `errors::bad_sequence()` — the lane's on-chain
/// ordering check. On this abort the message may ALREADY be applied (a
/// retry raced its own landed tx), so the deliverer re-checks the
/// on-chain seq before deciding.
pub const BAD_SEQUENCE_ABORT: u64 = 143;

/// Which lane direction a wire message type travels.
pub fn direction_for(msg_type: MsgType) -> &'static str {
    use crate::db::models::direction;
    match msg_type {
        MsgType::DepositNotice
        | MsgType::WithdrawRequest
        | MsgType::PayoutReceipt
        | MsgType::StateSync => direction::SPOKE_TO_HUB,
        MsgType::DepositAck | MsgType::WithdrawAck | MsgType::ConfigSync => {
            direction::HUB_TO_SPOKE
        }
    }
}

/// Strict in-order delivery: only `last_confirmed + 1` may go. Anything
/// later is held back until the gap fills (out-of-order arrival), and
/// anything at or below the confirmed watermark is a stale duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderGate {
    /// This is the next message on the lane — deliver it.
    Deliver,
    /// A predecessor is still missing/undelivered — hold.
    HoldBack,
    /// Already at/behind the confirmed watermark — confirm as duplicate.
    StaleDuplicate,
}

pub fn order_gate(last_confirmed: u64, seq: u64) -> OrderGate {
    if seq <= last_confirmed {
        OrderGate::StaleDuplicate
    } else if seq == last_confirmed + 1 {
        OrderGate::Deliver
    } else {
        OrderGate::HoldBack
    }
}

/// Capped exponential backoff: `base · 2^(attempts-1)`, capped.
pub fn backoff_delay_secs(attempts: i32, base_secs: u64, cap_secs: u64) -> u64 {
    if attempts <= 0 {
        return 0;
    }
    let shift = (attempts - 1).min(20) as u32;
    base_secs.saturating_mul(1u64 << shift).min(cap_secs)
}

/// Is a retry due, given when the last attempt was recorded?
pub fn retry_due(
    attempts: i32,
    updated_at: DateTime<Utc>,
    now: DateTime<Utc>,
    base_secs: u64,
    cap_secs: u64,
) -> bool {
    let delay = backoff_delay_secs(attempts, base_secs, cap_secs);
    now - updated_at >= Duration::seconds(delay as i64)
}

/// Extract the Move abort code from a submit error's rendered chain, e.g.
/// `… MoveAbort(MoveLocation { module: …, function: 3, instruction: 21 }, 143) …`.
pub fn move_abort_code(err_msg: &str) -> Option<u64> {
    let start = err_msg.find("MoveAbort(")?;
    let rest = &err_msg[start..];
    // The code is the number between the last "}, " inside the MoveAbort
    // and its closing ')'.
    let brace = rest.rfind("},")?;
    let tail = &rest[brace + 2..];
    let digits: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub fn is_bad_sequence(err_msg: &str) -> bool {
    move_abort_code(err_msg) == Some(BAD_SEQUENCE_ABORT)
}

/// What to do with a failed delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureAction {
    /// The receiving chain has already applied this seq (benign race —
    /// our earlier attempt landed, or someone else delivered): mark
    /// confirmed, suppress the alert.
    AlreadyApplied,
    /// Transient/unknown: bump attempts and retry with backoff.
    Retry,
    /// Attempt budget exhausted: terminal `failed` + tx-failed alert.
    Terminal,
}

/// Classify a delivery failure. `chain_applied_seq` is the receiver's
/// last-applied inbound seq, re-read AFTER the failure (the multichain
/// plan's bad_sequence discipline: only an on-chain re-check may declare
/// "already applied").
pub fn classify_failure(
    seq: u64,
    chain_applied_seq: Option<u64>,
    attempts_after_this: i32,
    max_attempts: i32,
) -> FailureAction {
    if let Some(applied) = chain_applied_seq {
        if applied >= seq {
            return FailureAction::AlreadyApplied;
        }
    }
    if attempts_after_this >= max_attempts {
        FailureAction::Terminal
    } else {
        FailureAction::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_gate_holds_back_out_of_order_arrivals() {
        // Lane at 4: 5 goes, 6/7 wait, 3 is a stale duplicate.
        assert_eq!(order_gate(4, 5), OrderGate::Deliver);
        assert_eq!(order_gate(4, 6), OrderGate::HoldBack);
        assert_eq!(order_gate(4, 7), OrderGate::HoldBack);
        assert_eq!(order_gate(4, 4), OrderGate::StaleDuplicate);
        assert_eq!(order_gate(4, 3), OrderGate::StaleDuplicate);
        // Fresh lane: the first message is seq 1.
        assert_eq!(order_gate(0, 1), OrderGate::Deliver);
        assert_eq!(order_gate(0, 2), OrderGate::HoldBack);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_delay_secs(0, 10, 600), 0);
        assert_eq!(backoff_delay_secs(1, 10, 600), 10);
        assert_eq!(backoff_delay_secs(2, 10, 600), 20);
        assert_eq!(backoff_delay_secs(3, 10, 600), 40);
        assert_eq!(backoff_delay_secs(7, 10, 600), 600); // capped
        assert_eq!(backoff_delay_secs(100, 10, 600), 600); // no overflow
    }

    #[test]
    fn retry_due_respects_the_delay() {
        let now = Utc::now();
        let updated = now - Duration::seconds(15);
        assert!(retry_due(1, updated, now, 10, 600)); // 10s due after 15s
        assert!(!retry_due(2, updated, now, 10, 600)); // 20s not yet
        assert!(retry_due(0, now, now, 10, 600)); // first attempt: immediate
    }

    #[test]
    fn parses_the_move_abort_code() {
        let msg = "handle_deposit_notice reverted: Failure { error: MoveAbort(MoveLocation \
                   { module: ModuleId { address: 0x5040, name: Identifier(\"spoke\") }, \
                   function: 3, instruction: 21 }, 143) }";
        assert_eq!(move_abort_code(msg), Some(143));
        assert!(is_bad_sequence(msg));

        let other = msg.replace("143", "111");
        assert_eq!(move_abort_code(&other), Some(111));
        assert!(!is_bad_sequence(&other));

        assert_eq!(move_abort_code("connection refused"), None);
        assert!(!is_bad_sequence("gRPC ExecuteTransaction: Unavailable"));
    }

    #[test]
    fn failure_classification_state_machine() {
        // bad_sequence + chain already at/past the seq → benign, confirm.
        assert_eq!(classify_failure(5, Some(5), 1, 8), FailureAction::AlreadyApplied);
        assert_eq!(classify_failure(5, Some(9), 1, 8), FailureAction::AlreadyApplied);
        // Chain behind the seq → real failure; retries until the budget.
        assert_eq!(classify_failure(5, Some(4), 1, 8), FailureAction::Retry);
        assert_eq!(classify_failure(5, Some(4), 8, 8), FailureAction::Terminal);
        // No chain read available → same retry/terminal split.
        assert_eq!(classify_failure(5, None, 2, 8), FailureAction::Retry);
        assert_eq!(classify_failure(5, None, 8, 8), FailureAction::Terminal);
    }

    #[test]
    fn directions_split_by_msg_type() {
        use crate::db::models::direction::{HUB_TO_SPOKE, SPOKE_TO_HUB};
        assert_eq!(direction_for(MsgType::DepositNotice), SPOKE_TO_HUB);
        assert_eq!(direction_for(MsgType::WithdrawRequest), SPOKE_TO_HUB);
        assert_eq!(direction_for(MsgType::PayoutReceipt), SPOKE_TO_HUB);
        assert_eq!(direction_for(MsgType::StateSync), SPOKE_TO_HUB);
        assert_eq!(direction_for(MsgType::DepositAck), HUB_TO_SPOKE);
        assert_eq!(direction_for(MsgType::WithdrawAck), HUB_TO_SPOKE);
        assert_eq!(direction_for(MsgType::ConfigSync), HUB_TO_SPOKE);
    }
}
