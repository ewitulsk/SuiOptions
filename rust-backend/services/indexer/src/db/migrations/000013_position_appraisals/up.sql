-- SO-304: per-position marks + consumed-appraisal NAV, fed from the new
-- PositionAppraised / VaultAppraised events. Last-write-wins fields, so
-- checkpoint replays are safe.

ALTER TABLE trading_vault_positions
    ADD COLUMN last_value BIGINT,
    ADD COLUMN last_appraised_at_ms BIGINT;

ALTER TABLE trading_vaults
    ADD COLUMN latest_nav NUMERIC(39),
    ADD COLUMN nav_updated_at_ms BIGINT;
