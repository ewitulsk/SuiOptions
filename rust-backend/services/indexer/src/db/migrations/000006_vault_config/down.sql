ALTER TABLE vaults
    DROP COLUMN mgmt_fee_bps_annual,
    DROP COLUMN perf_fee_bps,
    DROP COLUMN round_ms,
    DROP COLUMN selling_window_ms,
    DROP COLUMN min_strike_bps_over_spot,
    DROP COLUMN max_strike_bps_over_spot;
