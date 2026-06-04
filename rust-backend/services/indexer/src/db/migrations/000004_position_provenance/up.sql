-- SO-97: denormalize WriteExecuted provenance onto the position row so the
-- GraphQL `positions(objectIds)` query resolves from a single positions x
-- buckets join — no JSONB scan of indexed_events. Captured once at mint
-- (positions are never split, so object_id is a stable 1:1 key to
-- WriteExecuted.position_id).
--
--   premium_received — gross premium the writer received (settlement units, u64)
--   mm_account_id    — WriteExecuted.signer_account_id (the counterparty MM)
--   tx_digest        — the minting tx's digest (for explorer links)
--   minted_at_ms     — checkpoint timestamp at mint
--
-- Defaults let the columns be added cleanly; dropped immediately so future
-- inserts must supply real values.
ALTER TABLE positions ADD COLUMN premium_received NUMERIC(39) NOT NULL DEFAULT 0;
ALTER TABLE positions ADD COLUMN mm_account_id    TEXT        NOT NULL DEFAULT '';
ALTER TABLE positions ADD COLUMN tx_digest        TEXT        NOT NULL DEFAULT '';
ALTER TABLE positions ADD COLUMN minted_at_ms     BIGINT      NOT NULL DEFAULT 0;

ALTER TABLE positions ALTER COLUMN premium_received DROP DEFAULT;
ALTER TABLE positions ALTER COLUMN mm_account_id    DROP DEFAULT;
ALTER TABLE positions ALTER COLUMN tx_digest        DROP DEFAULT;
ALTER TABLE positions ALTER COLUMN minted_at_ms     DROP DEFAULT;
