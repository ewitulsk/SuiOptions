//! Slice schedule (README §6), restart-safe by construction: nothing is
//! remembered between ticks. The keeper runs **one auction at a time**;
//! whenever none is open inside the selling window, the next slice is
//! `deployable / remaining_slots`, where the remaining stagger slots are
//! recomputed from the clock (`(time_left / stagger) + 1`, capped at the
//! configured slice count). Sold slices shrink `deployable`, so amounts
//! adapt; unsold collateral returns to `deployable` at settle and is
//! naturally re-offered while the window is open.

/// Cap the configured stagger so at least `slices` slots fit inside the
/// vault's selling window. Returns `min(configured, selling_window_ms /
/// slices)`, floored at 1ms.
///
/// Weekly vaults (12h window, 90-min stagger) are unaffected — the cap only
/// bites when `stagger × slices` exceeds the window, e.g. an hourly round whose
/// ~40-min window would otherwise collapse `next_slice_amount` to a single
/// slot. Self-scaling, so one keeper can drive both cadences with one config.
pub fn effective_stagger_ms(
    configured_stagger_ms: u64,
    selling_window_ms: u64,
    slices: u64,
) -> u64 {
    let window_fit = (selling_window_ms / slices.max(1)).max(1);
    configured_stagger_ms.min(window_fit)
}

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

    #[test]
    fn effective_stagger_leaves_weekly_untouched() {
        // 12h window, 4 slices → 3h fit; configured 90-min stagger is smaller,
        // so the weekly path is unchanged.
        let window = 12 * HOUR;
        assert_eq!(effective_stagger_ms(90 * 60_000, window, 4), 90 * 60_000);
    }

    #[test]
    fn effective_stagger_shrinks_for_short_windows() {
        // ~40-min hourly selling window, 4 slices → 10-min fit, well below the
        // configured 90-min stagger, so 4 slots still open across the window.
        let window = 40 * 60_000;
        let stagger = effective_stagger_ms(90 * 60_000, window, 4);
        assert_eq!(stagger, 10 * 60_000);
        // With this stagger, the window opens the full slice count at the start.
        let slots = next_slice_amount(1_000_000, 0, window, stagger, 4, u64::MAX);
        assert_eq!(slots, Some(250_000));
    }

    #[test]
    fn effective_stagger_floors_at_one() {
        assert_eq!(effective_stagger_ms(90 * 60_000, 0, 4), 1);
        assert_eq!(effective_stagger_ms(90 * 60_000, 1_000_000, 0), 1_000_000);
    }
}
