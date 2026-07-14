//! Pure reconciliation decision logic.
//!
//! The reconciler resolves `needs_reconciliation` rows in two steps, per the
//! service guide:
//!
//! 1. **`getSignatureStatuses` first** — on Solana the recorded signature of
//!    the ambiguous tx is definitive: a status with an `err` means the tx
//!    landed and failed; a clean status means it landed.
//! 2. **Then the indexer** — a row whose planned bucket PDAs all appear in
//!    finalized `BucketCreated` events is confirmed (handled by the confirm
//!    pass before this decision runs). A row still unconfirmed once the
//!    indexer head has provably caught up past the submit anchor + safety
//!    margin never fully landed — clear it so the next tick re-claims and
//!    resumes (deterministic salts make the resume idempotent: buckets that
//!    do exist collide "already in use", classified Benign).
//!
//! Kept chain-free so every branch is unit-testable.

/// What `getSignatureStatuses` said about the recorded signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigStatus {
    /// No status (never landed, or aged out of the recent-status window).
    NotFound,
    /// Landed with no error.
    Landed,
    /// Landed and failed on-chain.
    Failed,
}

/// What to do with a needs_reconciliation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Row's buckets all landed — the confirm pass flips it to confirmed.
    Confirm,
    /// The roll provably never fully landed (tx failed, or the indexer
    /// caught up with buckets missing). Delete the row so the next tick
    /// re-claims the slot and resumes idempotently.
    Delete,
    /// Not enough information yet — leave the row for the next pass.
    Wait,
}

/// Decide the fate of a needs_reconciliation row.
///
/// `all_buckets_confirmed`: every planned bucket PDA is visible finalized on
/// the indexer. `sig`: status of the recorded (last attempted) signature.
/// `head_seq` vs `anchor + safety_margin`: has the indexer provably ingested
/// past the submit point?
pub fn decide(
    all_buckets_confirmed: bool,
    sig: SigStatus,
    head_seq: u64,
    anchor_seq: u64,
    safety_margin: u64,
) -> ReconcileAction {
    if all_buckets_confirmed {
        return ReconcileAction::Confirm;
    }
    match sig {
        // Definitive: the ambiguous tx landed and FAILED — that strike (and
        // everything after it) never happened. Resume via re-claim.
        SigStatus::Failed => ReconcileAction::Delete,
        // Landed-clean but the family isn't fully confirmed (partial roll:
        // the ambiguous tx was mid-family and the loop aborted), or not
        // found at all: fall back to the indexer-anchor rule.
        SigStatus::Landed | SigStatus::NotFound => {
            if head_seq > anchor_seq.saturating_add(safety_margin) {
                ReconcileAction::Delete
            } else {
                ReconcileAction::Wait
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_buckets_win_over_everything() {
        for sig in [SigStatus::NotFound, SigStatus::Landed, SigStatus::Failed] {
            assert_eq!(decide(true, sig, 0, 0, 100), ReconcileAction::Confirm);
        }
    }

    #[test]
    fn failed_signature_is_definitive_delete() {
        // Even when the indexer hasn't caught up yet.
        assert_eq!(
            decide(false, SigStatus::Failed, 0, 1_000, 100),
            ReconcileAction::Delete
        );
    }

    #[test]
    fn not_found_waits_for_anchor_then_deletes() {
        // Head hasn't passed anchor + margin → wait.
        assert_eq!(
            decide(false, SigStatus::NotFound, 1_050, 1_000, 100),
            ReconcileAction::Wait
        );
        // Exactly at anchor + margin → still wait (strictly greater clears).
        assert_eq!(
            decide(false, SigStatus::NotFound, 1_100, 1_000, 100),
            ReconcileAction::Wait
        );
        // Past it → the roll never landed; delete for re-claim.
        assert_eq!(
            decide(false, SigStatus::NotFound, 1_101, 1_000, 100),
            ReconcileAction::Delete
        );
    }

    #[test]
    fn landed_but_unconfirmed_is_partial_roll_delete_after_anchor() {
        // The ambiguous tx landed but the family is incomplete (loop aborted
        // mid-roll). Once the indexer is provably past the submit point, the
        // missing buckets will never appear — delete so the next tick
        // resumes; the landed buckets collide Benign on resubmit.
        assert_eq!(
            decide(false, SigStatus::Landed, 900, 1_000, 100),
            ReconcileAction::Wait
        );
        assert_eq!(
            decide(false, SigStatus::Landed, 2_000, 1_000, 100),
            ReconcileAction::Delete
        );
    }

    #[test]
    fn anchor_overflow_saturates() {
        assert_eq!(
            decide(false, SigStatus::NotFound, u64::MAX, u64::MAX, 100),
            ReconcileAction::Wait
        );
    }
}
