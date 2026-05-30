-- Add the on-chain Position object id to `positions`. Captured from
-- `WriteExecuted.position_id` (added when the contract started emitting
-- the freshly-minted object id alongside the recipient). Required by the
-- dashboard so the frontend can build a `redeem_position` PTB.
--
-- Backfill of historical rows: we deliberately don't try to reconstruct
-- ids for positions minted before the contract change — those rows get
-- the empty string and the api-service treats them as "object id
-- unknown, redeem via Sui RPC lookup". The intent is that all production
-- positions from here forward will have a real id, and historical rows
-- can be left as-is or wiped at the next indexer reset.

ALTER TABLE positions ADD COLUMN object_id TEXT NOT NULL DEFAULT '';

-- Drop the default so future inserts MUST provide a real id.
ALTER TABLE positions ALTER COLUMN object_id DROP DEFAULT;

CREATE INDEX positions_object_id_idx ON positions (object_id);
