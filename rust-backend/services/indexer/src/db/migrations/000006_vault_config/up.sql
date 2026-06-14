-- Active VaultConfig snapshot (consumer-facing subset) materialized onto the
-- vaults view from VaultCreated (genesis) and VaultConfigApplied (per finalize).
-- Nullable: a vault indexed before the config-carrying events exist stays null.
ALTER TABLE vaults
    ADD COLUMN mgmt_fee_bps_annual      BIGINT,
    ADD COLUMN perf_fee_bps             BIGINT,
    ADD COLUMN round_ms                 BIGINT,
    ADD COLUMN selling_window_ms        BIGINT,
    ADD COLUMN min_strike_bps_over_spot BIGINT,
    ADD COLUMN max_strike_bps_over_spot BIGINT;
