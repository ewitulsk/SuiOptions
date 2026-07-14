//! Expiry-cadence picker and per-tick roll/skip decision.
//!
//! Verbatim port of the Sui twin's `schedule.rs` — pure functions, no chain
//! types. MVP: fixed interval. `next_expiry_ms(latest_expiry_ms,
//! interval_ms)` is `latest + interval`. If the chain has no family for the
//! pair yet (cold start), we anchor at the next interval-aligned boundary
//! strictly after `now_ms`.
//!
//! [`decide_tick`] sits on top of that and answers the only question the
//! tick loop's pair-stanza in `main.rs` cares about: given the DB's latest
//! expiry and the wall clock, should we roll, and to what expiry?

/// Compute the next expiry to roll for a pair.
///
/// `latest_expiry_ms`:
///   - `Some(t)` — the DB already has a family for this pair; return
///     `t + interval_ms` (always strictly in the future relative to `t`).
///   - `None` — cold start. Return an epoch-aligned expiry that is
///     strictly more than `roll_threshold_ms` away from `now_ms`, so
///     the *next* tick doesn't immediately re-fire and double-roll.
pub fn next_expiry_ms(
    latest_expiry_ms: Option<u64>,
    interval_ms: u64,
    now_ms: u64,
    roll_threshold_ms: u64,
) -> u64 {
    if interval_ms == 0 {
        // Defensive — config validates this elsewhere. Never roll twice on
        // the same epoch.
        return now_ms.saturating_add(1);
    }
    match latest_expiry_ms {
        Some(t) => t.saturating_add(interval_ms),
        None => {
            // Cold start: land the expiry past the roll threshold so the
            // next tick doesn't immediately re-fire and double-roll.
            let target = now_ms.saturating_add(roll_threshold_ms);
            let k = target / interval_ms + 1;
            k.saturating_mul(interval_ms)
        }
    }
}

/// Per-pair decision the tick loop makes before any I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickDecision {
    /// Submit a new family at this expiry. Caller still has to resolve
    /// spot + grid + actually call the chain.
    Roll { next_expiry_ms: u64 },
    /// No work for this pair on this tick.
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Latest family is still further out than `roll_threshold_ms`. The
    /// stored value is `latest_expiry_ms - now_ms` (saturating), useful for
    /// the operator log.
    BeyondRollWindow { time_to_expiry_ms: u64 },
    /// Belt-and-braces guard for the case where the cadence picker would
    /// return an expiry the chain already has — should never fire on the
    /// MVP's fixed-interval cadence with `interval_ms > 0`, but does fire
    /// if config slips a zero interval through, so it stays.
    AlreadyAtComputedExpiry,
}

/// Decide whether a pair needs a fresh family this tick, and at what
/// expiry. Pure: takes `now_ms` and the DB's view of the latest expiry,
/// returns a decision. All chain/network I/O lives in the caller.
pub fn decide_tick(
    latest_expiry_ms: Option<u64>,
    now_ms: u64,
    roll_threshold_ms: u64,
    expiry_interval_ms: u64,
) -> TickDecision {
    let needs_roll = match latest_expiry_ms {
        None => true,
        Some(t) => t.saturating_sub(now_ms) < roll_threshold_ms,
    };
    if !needs_roll {
        // Unwrap is safe: the `None` arm above sets needs_roll = true.
        let t = latest_expiry_ms.expect("None arm sets needs_roll");
        return TickDecision::Skip(SkipReason::BeyondRollWindow {
            time_to_expiry_ms: t.saturating_sub(now_ms),
        });
    }
    let next = next_expiry_ms(latest_expiry_ms, expiry_interval_ms, now_ms, roll_threshold_ms);
    if let Some(t) = latest_expiry_ms {
        if t >= next {
            return TickDecision::Skip(SkipReason::AlreadyAtComputedExpiry);
        }
    }
    TickDecision::Roll { next_expiry_ms: next }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_after_existing_family() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let t = 1_700_000_000_000u64;
        assert_eq!(next_expiry_ms(Some(t), week, 0, week), t + week);
    }

    #[test]
    fn cold_start_lands_past_roll_threshold() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let now = 5 * week + 1;
        let next = next_expiry_ms(None, week, now, week);
        // Must be strictly past now + roll_threshold so the next tick
        // sees `next - now > threshold` and skips.
        assert!(next > now + week, "cold-start expiry must be past roll_threshold");
        // Must also be epoch-aligned.
        assert_eq!(next % week, 0);
    }

    #[test]
    fn cold_start_exactly_on_boundary_skips_to_next() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let now = 5 * week;
        let next = next_expiry_ms(None, week, now, week);
        // now + threshold = 6*week; k = 6*week/week + 1 = 7 → 7*week
        assert_eq!(next, 7 * week);
    }

    // -- decide_tick ---------------------------------------------------

    const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
    const ROLL_THRESHOLD: u64 = WEEK_MS; // default in config.rs

    #[test]
    fn decide_cold_start_rolls_past_threshold() {
        // No family yet → roll, expiry lands past now + roll_threshold.
        let now = 5 * WEEK_MS + 1;
        let d = decide_tick(None, now, ROLL_THRESHOLD, WEEK_MS);
        // With threshold == interval, target = now + week ≈ 6*week+1,
        // k = floor((6*week+1)/week)+1 = 7 → 7*week.
        assert_eq!(d, TickDecision::Roll { next_expiry_ms: 7 * WEEK_MS });
    }

    #[test]
    fn cold_start_does_not_double_roll() {
        // After the first cold-start roll lands at 7*WEEK_MS, the next
        // tick (now latest=Some(7*WEEK_MS)) should skip because
        // 7w - (5w+1) > threshold.
        let now = 5 * WEEK_MS + 1;
        let cold_expiry = 7 * WEEK_MS; // what the first roll would produce
        let d = decide_tick(Some(cold_expiry), now, ROLL_THRESHOLD, WEEK_MS);
        assert!(matches!(d, TickDecision::Skip(SkipReason::BeyondRollWindow { .. })));
    }

    #[test]
    fn decide_skips_when_latest_is_beyond_window() {
        // Latest family expires in 2 weeks, threshold is 1 week → skip.
        let now = 1_700_000_000_000u64;
        let latest = now + 2 * WEEK_MS;
        let d = decide_tick(Some(latest), now, ROLL_THRESHOLD, WEEK_MS);
        assert_eq!(
            d,
            TickDecision::Skip(SkipReason::BeyondRollWindow {
                time_to_expiry_ms: 2 * WEEK_MS,
            })
        );
    }

    #[test]
    fn decide_rolls_inside_window() {
        // Latest expires in 3 days; threshold is 1 week → roll, expiry = latest + 1w.
        let now = 1_700_000_000_000u64;
        let latest = now + 3 * 24 * 60 * 60 * 1_000;
        let d = decide_tick(Some(latest), now, ROLL_THRESHOLD, WEEK_MS);
        assert_eq!(d, TickDecision::Roll { next_expiry_ms: latest + WEEK_MS });
    }

    #[test]
    fn decide_threshold_is_strict_less_than() {
        // `latest - now == threshold` exactly → not yet inside window.
        let now = 1_700_000_000_000u64;
        let latest = now + ROLL_THRESHOLD;
        let d = decide_tick(Some(latest), now, ROLL_THRESHOLD, WEEK_MS);
        assert_eq!(
            d,
            TickDecision::Skip(SkipReason::BeyondRollWindow {
                time_to_expiry_ms: ROLL_THRESHOLD,
            })
        );

        // One ms inside → roll.
        let d2 = decide_tick(Some(latest - 1), now, ROLL_THRESHOLD, WEEK_MS);
        assert!(matches!(d2, TickDecision::Roll { .. }));
    }

    #[test]
    fn decide_rolls_when_latest_already_past_now() {
        // Pathological: the latest family has already expired and nobody
        // has rolled. `saturating_sub` clamps to 0, which is < any
        // positive threshold → roll the next family.
        let now = 1_700_000_000_000u64;
        let latest = now - WEEK_MS;
        let d = decide_tick(Some(latest), now, ROLL_THRESHOLD, WEEK_MS);
        assert_eq!(d, TickDecision::Roll { next_expiry_ms: latest + WEEK_MS });
    }

    #[test]
    fn decide_no_double_roll_with_zero_interval_guard() {
        // Defensive: if a misconfigured zero interval reaches the planner,
        // `next_expiry_ms` returns `now + 1`. With latest > now, the
        // sanity check should fire and skip.
        let now = 1_700_000_000_000u64;
        let latest = now + 3 * 24 * 60 * 60 * 1_000;
        let d = decide_tick(Some(latest), now, ROLL_THRESHOLD, 0);
        assert_eq!(d, TickDecision::Skip(SkipReason::AlreadyAtComputedExpiry));
    }

    #[test]
    fn cold_start_with_small_threshold() {
        // When threshold < interval, the cold-start expiry still lands
        // past the threshold so no double-roll.
        let week = WEEK_MS;
        let threshold = week / 2; // half a week
        let now = 5 * week + 1;
        let next = next_expiry_ms(None, week, now, threshold);
        assert!(next > now + threshold);
        assert_eq!(next % week, 0);
    }
}
