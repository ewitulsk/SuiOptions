ALTER TABLE trading_vaults
    DROP COLUMN IF EXISTS external_equity_updated_at_ms,
    DROP COLUMN IF EXISTS latest_external_equity,
    DROP COLUMN IF EXISTS external_exposure,
    DROP COLUMN IF EXISTS external_account;
