//! Expiry-cadence picker.
//!
//! MVP: fixed interval. `next_expiry_ms(latest_expiry_ms, interval_ms)` is
//! `latest + interval`. If the chain has no family for the pair yet (cold
//! start), we anchor at the next interval-aligned boundary strictly after
//! `now_ms`.
//!
//! Future: cron-style ("next Friday 08:00 UTC") via a string spec in config.

/// Compute the next expiry to roll for a pair.
///
/// `latest_expiry_ms`:
///   - `Some(t)` — chain already has a family for this pair; return
///     `t + interval_ms` (always strictly in the future relative to `t`).
///   - `None` — cold start. Return the next `now_ms`-aligned multiple of
///     `interval_ms`, i.e. the smallest `interval_ms * k > now_ms`.
pub fn next_expiry_ms(
    latest_expiry_ms: Option<u64>,
    interval_ms: u64,
    now_ms: u64,
) -> u64 {
    if interval_ms == 0 {
        // Defensive — config validates this elsewhere. Never roll twice on
        // the same epoch.
        return now_ms.saturating_add(1);
    }
    match latest_expiry_ms {
        Some(t) => t.saturating_add(interval_ms),
        None => {
            // Cold start: next interval-aligned slot strictly after `now`.
            // Aligning to epoch (ms-since-Unix) keeps every cold-started
            // pair on the same global cadence.
            let k = now_ms / interval_ms + 1;
            k.saturating_mul(interval_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_after_existing_family() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let t = 1_700_000_000_000u64;
        assert_eq!(next_expiry_ms(Some(t), week, 0), t + week);
    }

    #[test]
    fn cold_start_aligns_to_epoch_grid() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let now = 5 * week + 1; // somewhere inside the 6th week-slot
        let next = next_expiry_ms(None, week, now);
        assert_eq!(next, 6 * week);
        assert!(next > now);
    }

    #[test]
    fn cold_start_exactly_on_boundary_skips_to_next() {
        let week = 7 * 24 * 60 * 60 * 1_000;
        let now = 5 * week;
        let next = next_expiry_ms(None, week, now);
        assert_eq!(next, 6 * week);
    }
}
