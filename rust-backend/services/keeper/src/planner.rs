//! The planner (README §4): a **pure function** from observed chain
//! state to the single action the vault needs next. Stateless by
//! design — it recomputes "what should exist by now" from the
//! [`VaultView`] alone, so keeper restarts and keeper races are
//! harmless. One action per tick keeps shared-object contention and
//! race handling trivial; settling ladders simply take a few ticks.

use sui_types::base_types::ObjectID;

use crate::slicing;
use crate::state::{RfqView, VaultView};

/// What the keeper knows about the round's current bucket (from the
/// indexer), beyond its id.
#[derive(Debug, Clone)]
pub struct BucketMeta {
    pub call_type: String,
    /// Bucket invalidated on-chain — auctions on it must take the
    /// `settle_rfq_expired` recovery path.
    pub invalidated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    CrankRedeem { bucket: ObjectID, call_type: String },
    SettleRfq { rfq: ObjectID, bucket: ObjectID, call_type: String },
    SettleRfqExpired { rfq: ObjectID, bucket: ObjectID, call_type: String },
    SwapProceeds { max_settlement_in: u64 },
    FinalizeRound,
    /// Active round, no bucket: the tick loop resolves a strike pick
    /// (σ, spot, candidates) and submits `select_bucket` — or falls back
    /// to `FinalizeRound` if queued flows are waiting and nothing is
    /// selectable (an idle round is finalizable immediately).
    SelectBucketNeeded,
    OpenRfq { bucket: ObjectID, call_type: String, slice_amount: u64 },
    /// Nothing to do this tick.
    Idle,
}

pub struct PlanInput<'a> {
    pub view: &'a VaultView,
    pub now_ms: u64,
    /// Live (object-still-exists) vault-coupled auctions.
    pub auctions: &'a [RfqView],
    /// Meta for `view.current_bucket`, when one is selected and known.
    pub bucket_meta: Option<&'a BucketMeta>,
    pub stagger_ms: u64,
    pub max_slices: u64,
}

pub fn plan(input: &PlanInput<'_>) -> Action {
    let v = input.view;
    let now = input.now_ms;

    // A selected bucket whose expiry passed is settling in all but name:
    // the first crank calls `maybe_enter_settling` on the way in.
    let settling = v.settling || (v.current_bucket.is_some() && now >= v.current_expiry_ms);

    if settling {
        if v.pending_positions > 0 {
            let (Some(bucket), Some(meta)) = (v.current_bucket, input.bucket_meta) else {
                // Positions without a known bucket: state/indexer lag.
                return Action::Idle;
            };
            return Action::CrankRedeem { bucket, call_type: meta.call_type.clone() };
        }
        if let Some(action) = settle_due_auction(input, /* require_closed */ false) {
            return action;
        }
        if v.open_rfqs > 0 {
            // The vault counts an auction we haven't discovered yet
            // (event lag) — wait rather than mis-finalize.
            return Action::Idle;
        }
        if v.proceeds_settlement > 0 && !v.config.hold_premium_in_settlement {
            if v.config.deepbook_pool_id.is_none() {
                // finalize would abort vault_proceeds_unswapped; this is
                // a vault-config problem the keeper can't fix.
                return Action::Idle;
            }
            return Action::SwapProceeds { max_settlement_in: v.proceeds_settlement };
        }
        return Action::FinalizeRound;
    }

    // Active, pre-expiry.
    let Some(bucket) = v.current_bucket else {
        return Action::SelectBucketNeeded;
    };
    // Auctions past their deadline resolve mid-round (premium compounds
    // into the next slice's deployable).
    if let Some(action) = settle_due_auction(input, /* require_closed */ true) {
        return action;
    }
    if v.open_rfqs == 0 && now < v.selling_ends_ms {
        if let Some(meta) = input.bucket_meta {
            if let Some(slice_amount) = slicing::next_slice_amount(
                v.deployable,
                now,
                v.selling_ends_ms,
                input.stagger_ms,
                input.max_slices,
                v.config.max_slice_amount,
            ) {
                return Action::OpenRfq {
                    bucket,
                    call_type: meta.call_type.clone(),
                    slice_amount,
                };
            }
        }
    }
    // Mid-round premium conversion: keeps proceeds compounding into
    // later slices instead of waiting for settling.
    if v.proceeds_settlement > 0
        && !v.config.hold_premium_in_settlement
        && v.config.deepbook_pool_id.is_some()
    {
        return Action::SwapProceeds { max_settlement_in: v.proceeds_settlement };
    }
    Action::Idle
}

/// First discovered auction that needs resolving. During settling
/// (`require_closed == false`) an invalidated/expired bucket routes to
/// the recovery path; a deadline that passed settles normally.
fn settle_due_auction(input: &PlanInput<'_>, require_closed: bool) -> Option<Action> {
    let meta = input.bucket_meta?;
    for rfq in input.auctions {
        let recovery = meta.invalidated && !require_closed;
        if recovery {
            return Some(Action::SettleRfqExpired {
                rfq: rfq.rfq_id,
                bucket: rfq.bucket_id,
                call_type: meta.call_type.clone(),
            });
        }
        if input.now_ms >= rfq.deadline_ms {
            return Some(Action::SettleRfq {
                rfq: rfq.rfq_id,
                bucket: rfq.bucket_id,
                call_type: meta.call_type.clone(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{VaultConfigView, VaultView};

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_700_000_000_000;

    fn id(n: u8) -> ObjectID {
        ObjectID::from_single_byte(n)
    }

    fn cfg() -> VaultConfigView {
        VaultConfigView {
            min_strike_bps_over_spot: 300,
            max_strike_bps_over_spot: 6000,
            min_expiry_lead_ms: 3 * 24 * HOUR,
            max_expiry_lead_ms: 9 * 24 * HOUR,
            max_slice_amount: u64::MAX,
            max_open_rfqs: 4,
            round_ms: 7 * 24 * HOUR,
            hold_premium_in_settlement: false,
            deepbook_pool_id: Some(id(0xd1)),
            underlying_feed_id: vec![0u8; 32],
            settlement_feed_id: vec![0u8; 32],
            underlying_decimals: 9,
            settlement_decimals: 6,
        }
    }

    /// A mid-round active vault with a selected bucket.
    fn active_view() -> VaultView {
        VaultView {
            round: 3,
            settling: false,
            current_bucket: Some(id(0xb1)),
            current_expiry_ms: NOW + 5 * 24 * HOUR,
            selling_ends_ms: NOW + 6 * HOUR,
            open_rfqs: 0,
            pending_positions: 0,
            deployable: 1_000_000,
            proceeds_settlement: 0,
            pending_deposits: 0,
            queued_withdraw_shares: 0,
            config: cfg(),
        }
    }

    fn settling_view() -> VaultView {
        VaultView { settling: true, ..active_view() }
    }

    fn meta() -> BucketMeta {
        BucketMeta { call_type: "0xc::call_2::CALL_2".into(), invalidated: false }
    }

    fn plan_with(view: &VaultView, auctions: &[RfqView], meta: Option<&BucketMeta>) -> Action {
        plan(&PlanInput {
            view,
            now_ms: NOW,
            auctions,
            bucket_meta: meta,
            stagger_ms: 90 * 60_000,
            max_slices: 4,
        })
    }

    fn rfq(deadline_ms: u64) -> RfqView {
        RfqView { rfq_id: id(0xaa), bucket_id: id(0xb1), deadline_ms, amount: 250_000 }
    }

    // ── settling ladder ────────────────────────────────────────────

    #[test]
    fn settling_redeems_positions_first() {
        let mut v = settling_view();
        v.pending_positions = 2;
        v.proceeds_settlement = 999;
        let m = meta();
        assert_eq!(
            plan_with(&v, &[], Some(&m)),
            Action::CrankRedeem { bucket: id(0xb1), call_type: m.call_type.clone() }
        );
    }

    #[test]
    fn settling_then_settles_closed_auctions() {
        let mut v = settling_view();
        v.open_rfqs = 1;
        let m = meta();
        let a = [rfq(NOW - 1)];
        assert_eq!(
            plan_with(&v, &a, Some(&m)),
            Action::SettleRfq { rfq: id(0xaa), bucket: id(0xb1), call_type: m.call_type.clone() }
        );
    }

    #[test]
    fn settling_routes_invalidated_bucket_to_recovery() {
        let mut v = settling_view();
        v.open_rfqs = 1;
        let m = BucketMeta { invalidated: true, ..meta() };
        // Even an auction whose deadline hasn't passed: the bucket died.
        let a = [rfq(NOW + HOUR)];
        assert!(matches!(plan_with(&v, &a, Some(&m)), Action::SettleRfqExpired { .. }));
    }

    #[test]
    fn settling_waits_for_undiscovered_auctions() {
        let mut v = settling_view();
        v.open_rfqs = 1; // chain says one exists; we found none
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::Idle);
    }

    #[test]
    fn settling_swaps_then_finalizes() {
        let mut v = settling_view();
        v.proceeds_settlement = 4_200;
        assert_eq!(
            plan_with(&v, &[], Some(&meta())),
            Action::SwapProceeds { max_settlement_in: 4_200 }
        );
        v.proceeds_settlement = 0;
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::FinalizeRound);
    }

    #[test]
    fn hold_premium_vaults_skip_the_swap() {
        let mut v = settling_view();
        v.proceeds_settlement = 4_200;
        v.config.hold_premium_in_settlement = true;
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::FinalizeRound);
    }

    #[test]
    fn unswappable_proceeds_idle_not_finalize() {
        // No pinned pool + proceeds + hold=false: finalize would abort.
        let mut v = settling_view();
        v.proceeds_settlement = 4_200;
        v.config.deepbook_pool_id = None;
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::Idle);
    }

    #[test]
    fn genesis_settling_finalizes_immediately() {
        let mut v = settling_view();
        v.current_bucket = None;
        v.current_expiry_ms = 0;
        assert_eq!(plan_with(&v, &[], None), Action::FinalizeRound);
    }

    #[test]
    fn expiry_passed_counts_as_settling_even_while_active() {
        let mut v = active_view();
        v.current_expiry_ms = NOW - 1; // bucket expired, phase not flipped
        v.pending_positions = 1;
        let m = meta();
        assert!(matches!(plan_with(&v, &[], Some(&m)), Action::CrankRedeem { .. }));
    }

    // ── active round ───────────────────────────────────────────────

    #[test]
    fn active_without_bucket_asks_for_selection() {
        let mut v = active_view();
        v.current_bucket = None;
        v.current_expiry_ms = 0;
        assert_eq!(plan_with(&v, &[], None), Action::SelectBucketNeeded);
    }

    #[test]
    fn active_opens_the_next_slice() {
        let v = active_view();
        let m = meta();
        // 6h window, 90-min stagger, cap 4 → quarter of deployable.
        assert_eq!(
            plan_with(&v, &[], Some(&m)),
            Action::OpenRfq {
                bucket: id(0xb1),
                call_type: m.call_type.clone(),
                slice_amount: 250_000
            }
        );
    }

    #[test]
    fn active_settles_closed_auction_before_opening_another() {
        let mut v = active_view();
        v.open_rfqs = 1;
        let m = meta();
        let a = [rfq(NOW - 1)];
        assert!(matches!(plan_with(&v, &a, Some(&m)), Action::SettleRfq { .. }));
        // Auction still live → neither settle nor a second open.
        let a = [rfq(NOW + HOUR)];
        assert_eq!(plan_with(&v, &a, Some(&m)), Action::Idle);
    }

    #[test]
    fn active_after_window_swaps_midround_premium() {
        let mut v = active_view();
        v.selling_ends_ms = NOW - 1; // window over, holding to expiry
        v.proceeds_settlement = 999;
        assert_eq!(
            plan_with(&v, &[], Some(&meta())),
            Action::SwapProceeds { max_settlement_in: 999 }
        );
        v.proceeds_settlement = 0;
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::Idle);
    }

    #[test]
    fn zero_deployable_round_idles_until_expiry() {
        let mut v = active_view();
        v.deployable = 0;
        assert_eq!(plan_with(&v, &[], Some(&meta())), Action::Idle);
    }
}
