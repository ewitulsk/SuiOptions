-- Runtime market discovery (SO-416). `source` records who listed the row:
-- 'deployments' rows mirror the deployments record and are the only ones the
-- boot reconciliation may auto-disable; 'discovered' rows arrive from chain
-- MarketCreatedEvents (permissionless option listings) and survive redeploys.
-- `paused` mirrors the on-chain registry pause flag (PauseEvent) so intake
-- can reject instead of letting crossing orders fail settlement.
ALTER TABLE exchange_markets
    ADD COLUMN source TEXT    NOT NULL DEFAULT 'deployments',
    ADD COLUMN paused BOOLEAN NOT NULL DEFAULT FALSE;
