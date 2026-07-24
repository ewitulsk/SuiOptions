ALTER TABLE trading_vaults
    DROP COLUMN IF EXISTS nav_updated_at_ms,
    DROP COLUMN IF EXISTS latest_nav;

ALTER TABLE trading_vault_positions
    DROP COLUMN IF EXISTS last_appraised_at_ms,
    DROP COLUMN IF EXISTS last_value;
