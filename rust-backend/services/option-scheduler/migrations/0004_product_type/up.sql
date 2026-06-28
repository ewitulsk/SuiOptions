-- Product discriminator: a single (underlying, settlement, expiry, cadence)
-- slot can now host BOTH a covered-call family and a cash-secured-put family
-- simultaneously. Tag each roll with its product and widen the active-slot
-- uniqueness to include it so the two products never block each other.
ALTER TABLE scheduler_rolls
    ADD COLUMN product_type TEXT NOT NULL DEFAULT 'call'; -- existing rows are calls
-- Drop the default so future inserts must state their product explicitly.
ALTER TABLE scheduler_rolls
    ALTER COLUMN product_type DROP DEFAULT;

DROP INDEX IF EXISTS scheduler_rolls_active_slot;
CREATE UNIQUE INDEX scheduler_rolls_active_slot
    ON scheduler_rolls (underlying_symbol, settlement_symbol, expiry_ms, product_type)
    WHERE state IN ('pending', 'submitted', 'confirmed', 'needs_reconciliation');
