-- Solana port of option-scheduler's scheduler_rolls: tx_digest → signature
-- (base58 transaction signature of the roll's last landed create_bucket tx),
-- bucket_ids = the deterministically-derived bucket PDAs (base58), recorded
-- BEFORE submit so the reconciler can check them even for ambiguous rows.
CREATE TABLE scheduler_rolls (
    id                    BIGSERIAL PRIMARY KEY,
    underlying_symbol     TEXT       NOT NULL,
    settlement_symbol     TEXT       NOT NULL,
    expiry_ms             BIGINT     NOT NULL,
    expiry_interval_ms    BIGINT     NOT NULL,
    product_type          TEXT       NOT NULL,   -- call|put
    state                 TEXT       NOT NULL,   -- pending|submitted|confirmed|needs_reconciliation|superseded
    signature             TEXT       NULL,        -- last landed (or ambiguous) tx signature
    bucket_ids            JSONB      NULL,        -- derived bucket PDAs, base58, recorded pre-submit
    confirmed_bucket_ids  JSONB      NULL,        -- filled by indexer feedback
    retry_count           INT        NOT NULL DEFAULT 0,
    last_error            TEXT       NULL,
    submit_anchor_seq     BIGINT     NULL,        -- indexer sequence at time of submit
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Hard guarantee: at most ONE active row per slot, enforced by Postgres.
CREATE UNIQUE INDEX scheduler_rolls_active_slot
    ON scheduler_rolls (underlying_symbol, settlement_symbol, expiry_ms, product_type)
    WHERE state IN ('pending', 'submitted', 'confirmed', 'needs_reconciliation');
