DROP INDEX IF EXISTS scheduler_rolls_active_slot;
CREATE UNIQUE INDEX scheduler_rolls_active_slot
    ON scheduler_rolls (underlying_symbol, settlement_symbol, expiry_ms)
    WHERE state IN ('pending', 'submitted', 'confirmed', 'needs_reconciliation');
ALTER TABLE scheduler_rolls DROP COLUMN IF EXISTS product_type;
