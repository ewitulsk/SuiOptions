-- Cadence discriminator: allow two families/vaults for the SAME
-- (underlying, settlement) pair at different cadences (e.g. a weekly TSUI
-- vault and an hourly TSUI vault running side by side).
--
-- Rolls: `latest_active_expiry` previously keyed only on the pair, so a far
-- weekly expiry would suppress an hourly roll for the same pair. Tag each roll
-- with its cadence so the latest-expiry lookup is per-cadence. The active-slot
-- index already discriminates on `expiry_ms` (hourly and weekly expiries never
-- coincide), so it is left unchanged.
ALTER TABLE scheduler_rolls
    ADD COLUMN expiry_interval_ms BIGINT NOT NULL DEFAULT 604800000; -- weekly
-- Existing rows are the weekly cadence; the default backfilled them. Drop the
-- default so future inserts must state their cadence explicitly.
ALTER TABLE scheduler_rolls
    ALTER COLUMN expiry_interval_ms DROP DEFAULT;

-- Vaults: at most ONE live vault per (pair) becomes at most one per
-- (pair, round_ms), so the weekly vault no longer blocks creating the hourly
-- one. Existing vaults are weekly (the template default).
ALTER TABLE scheduler_vaults
    ADD COLUMN round_ms BIGINT NOT NULL DEFAULT 604800000; -- weekly
ALTER TABLE scheduler_vaults
    ALTER COLUMN round_ms DROP DEFAULT;

DROP INDEX IF EXISTS scheduler_vaults_active_pair;
CREATE UNIQUE INDEX scheduler_vaults_active_pair
    ON scheduler_vaults (underlying_symbol, settlement_symbol, round_ms)
    WHERE state IN ('pending', 'coin_published', 'confirmed');
