//! Slice schedule (README §6), restart-safe by construction: nothing is
//! remembered between ticks. The keeper runs **one auction at a time**;
//! whenever none is open inside the selling window, the next slice is
//! `deployable / remaining_slots`, where the remaining stagger slots are
//! recomputed from the clock (`(time_left / stagger) + 1`, capped at the
//! configured slice count). Sold slices shrink `deployable`, so amounts
//! adapt; unsold collateral returns to `deployable` at settle and is
//! naturally re-offered while the window is open.

/// Size of the slice to open now, or `None` when nothing should open
/// (window closed, nothing deployable, or an auction already running —
/// the caller checks `open_rfqs`).
pub fn next_slice_amount(
    deployable: u64,
    now_ms: u64,
    selling_ends_ms: u64,
    stagger_ms: u64,
    max_slices: u64,
    max_slice_amount: u64,
) -> Option<u64> {
    if deployable == 0 || now_ms >= selling_ends_ms || max_slices == 0 {
        return None;
    }
    let time_left = selling_ends_ms - now_ms;
    let slots = if stagger_ms == 0 {
        1
    } else {
        (time_left / stagger_ms + 1).min(max_slices).max(1)
    };
    Some((deployable / slots).max(1).min(max_slice_amount))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;
    const STAGGER: u64 = 90 * 60_000; // 90 min

    #[test]
    fn quarters_at_window_open_with_four_slots() {
        // 6h window, 90-min stagger → 5 raw slots, capped at 4.
        let amt = next_slice_amount(1_000_000, 0, 6 * HOUR, STAGGER, 4, u64::MAX);
        assert_eq!(amt, Some(250_000));
    }

    #[test]
    fn slots_shrink_as_the_window_closes() {
        // 100 min left → 2 slots → half of what's still deployable.
        let amt =
            next_slice_amount(600_000, 6 * HOUR - 100 * 60_000, 6 * HOUR, STAGGER, 4, u64::MAX);
        assert_eq!(amt, Some(300_000));
        // Final stretch (< stagger left) → everything remaining.
        let amt = next_slice_amount(600_000, 6 * HOUR - 10 * 60_000, 6 * HOUR, STAGGER, 4, u64::MAX);
        assert_eq!(amt, Some(600_000));
    }

    #[test]
    fn respects_the_vault_slice_cap() {
        let amt = next_slice_amount(1_000_000, 0, 6 * HOUR, STAGGER, 1, 150_000);
        assert_eq!(amt, Some(150_000));
    }

    #[test]
    fn idempotent_under_restart() {
        // Same chain state + same clock ⇒ same slice, no matter how many
        // times the keeper rebooted in between.
        let a = next_slice_amount(800_000, 2 * HOUR, 6 * HOUR, STAGGER, 4, u64::MAX);
        let b = next_slice_amount(800_000, 2 * HOUR, 6 * HOUR, STAGGER, 4, u64::MAX);
        assert_eq!(a, b);
    }

    #[test]
    fn nothing_to_do_cases() {
        assert_eq!(next_slice_amount(0, 0, 6 * HOUR, STAGGER, 4, u64::MAX), None);
        assert_eq!(next_slice_amount(1_000, 6 * HOUR, 6 * HOUR, STAGGER, 4, u64::MAX), None);
        assert_eq!(next_slice_amount(1_000, 7 * HOUR, 6 * HOUR, STAGGER, 4, u64::MAX), None);
        assert_eq!(next_slice_amount(1_000, 0, 6 * HOUR, STAGGER, 0, u64::MAX), None);
    }

    #[test]
    fn dust_still_offers_at_least_one_unit() {
        let amt = next_slice_amount(3, 0, 6 * HOUR, STAGGER, 4, u64::MAX);
        assert_eq!(amt, Some(1));
    }
}
