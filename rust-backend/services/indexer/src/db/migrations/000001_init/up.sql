-- Initial indexer schema.
--
-- Three families:
--   1. `indexer_progress`         — singleton row, last fully-processed checkpoint.
--   2. `indexed_events`           — append-only log mirroring the in-memory `Vec<IndexedEvent>`.
--   3. accounts / account_balances / buckets / positions — materialised views
--      mirroring `store::{AccountState, BucketState, PositionState}`.
--
-- The cursor model (range_start, range_end, total_written, exercise_cursor)
-- needs u128 headroom; we store those columns as NUMERIC(39) — 2^128 has 39
-- decimal digits.

CREATE TABLE indexer_progress (
    id              SMALLINT     PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_checkpoint BIGINT       NOT NULL,
    last_sequence   BIGINT       NOT NULL,
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE indexed_events (
    sequence      BIGINT       PRIMARY KEY,
    checkpoint    BIGINT       NOT NULL,
    tx_digest     TEXT         NOT NULL,
    event_index   INTEGER      NOT NULL,
    timestamp_ms  BIGINT       NOT NULL,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    UNIQUE (checkpoint, tx_digest, event_index)
);
CREATE INDEX indexed_events_event_type_idx ON indexed_events (event_type);
CREATE INDEX indexed_events_checkpoint_idx ON indexed_events (checkpoint);

CREATE TABLE accounts (
    account_id      TEXT         PRIMARY KEY,
    owner           TEXT,
    signing_pubkey  BYTEA        NOT NULL DEFAULT '\x',
    updated_at_seq  BIGINT       NOT NULL
);

CREATE TABLE account_balances (
    account_id      TEXT         NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    asset_type      TEXT         NOT NULL,
    balance         NUMERIC(39)  NOT NULL,
    updated_at_seq  BIGINT       NOT NULL,
    PRIMARY KEY (account_id, asset_type)
);

CREATE TABLE buckets (
    bucket_id        TEXT         PRIMARY KEY,
    asset_type       TEXT         NOT NULL,
    settlement_type  TEXT         NOT NULL,
    -- u128 fits in 39 digits (u128::MAX = 340282366920938463463374607431768211455).
    -- Real ratio = strike / 10^strike_scale; see SO-55.
    strike           NUMERIC(39)  NOT NULL,
    strike_scale     SMALLINT     NOT NULL DEFAULT 0,
    expiry_ms        BIGINT       NOT NULL,
    total_written    NUMERIC(39)  NOT NULL DEFAULT 0,
    exercise_cursor  NUMERIC(39)  NOT NULL DEFAULT 0,
    cleaned          BOOLEAN      NOT NULL DEFAULT false,
    updated_at_seq   BIGINT       NOT NULL
);

CREATE TABLE positions (
    bucket_id        TEXT         NOT NULL,
    range_start      NUMERIC(39)  NOT NULL,
    range_end        NUMERIC(39)  NOT NULL,
    recipient        TEXT         NOT NULL,
    updated_at_seq   BIGINT       NOT NULL,
    PRIMARY KEY (bucket_id, range_start)
);
CREATE INDEX positions_recipient_idx ON positions (recipient);
