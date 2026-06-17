DROP INDEX IF EXISTS scheduler_vaults_active_pair;
CREATE UNIQUE INDEX scheduler_vaults_active_pair
    ON scheduler_vaults (underlying_symbol, settlement_symbol)
    WHERE state IN ('pending', 'coin_published', 'confirmed');
ALTER TABLE scheduler_vaults DROP COLUMN IF EXISTS round_ms;

ALTER TABLE scheduler_rolls DROP COLUMN IF EXISTS expiry_interval_ms;
