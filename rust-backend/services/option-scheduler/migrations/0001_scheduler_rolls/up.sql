CREATE TABLE scheduler_rolls (
    id                    BIGSERIAL PRIMARY KEY,
    underlying_symbol     TEXT       NOT NULL,
    settlement_symbol     TEXT       NOT NULL,
    expiry_ms             BIGINT     NOT NULL,
    state                 TEXT       NOT NULL,   -- pending|submitted|confirmed|needs_reconciliation
    tx_digest             TEXT       NULL,
    bucket_ids            JSONB      NULL,        -- from ObjectChanges
    confirmed_bucket_ids  JSONB      NULL,        -- filled by indexer feedback
    retry_count           INT        NOT NULL DEFAULT 0,
    last_error            TEXT       NULL,
    submit_anchor_seq     BIGINT     NULL,        -- indexer sequence at time of submit
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Hard guarantee: at most ONE active row per slot, enforced by Postgres.
CREATE UNIQUE INDEX scheduler_rolls_active_slot
    ON scheduler_rolls (underlying_symbol, settlement_symbol, expiry_ms)
    WHERE state IN ('pending', 'submitted', 'confirmed', 'needs_reconciliation');
