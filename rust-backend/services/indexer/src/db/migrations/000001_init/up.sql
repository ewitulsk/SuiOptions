-- Consolidated indexer schema (squashed 000001–000004).
--
-- The DB is not live on mainnet, so the prior incremental migrations
-- (init → bucket_invalidated → position_object_id → position_provenance)
-- were squashed into this single init. A fresh / wiped DB gets the final
-- shape in one step; this matches `src/db/schema.rs` exactly.
--
-- Four families:
--   1. `indexer_progress`  — singleton row, last fully-processed checkpoint.
--   2. `indexed_events`    — append-only log mirroring the in-memory log.
--   3. accounts / account_balances / buckets / positions — materialised
--      views mirroring `store::{AccountState, BucketState, PositionState}`.
--
-- u128 cursor fields (range_start/end, total_written, exercise_cursor,
-- strike, premium_received) are NUMERIC(39) — 2^128 has 39 decimal digits.

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
    -- Real ratio = strike / 10^strike_scale; see SO-55.
    strike           NUMERIC(39)  NOT NULL,
    strike_scale     SMALLINT     NOT NULL DEFAULT 0,
    expiry_ms        BIGINT       NOT NULL,
    total_written    NUMERIC(39)  NOT NULL DEFAULT 0,
    exercise_cursor  NUMERIC(39)  NOT NULL DEFAULT 0,
    cleaned          BOOLEAN      NOT NULL DEFAULT false,
    -- SO-69: admin freeze on new writes. Exercise/redeem unaffected.
    invalidated      BOOLEAN      NOT NULL DEFAULT false,
    updated_at_seq   BIGINT       NOT NULL
);

CREATE TABLE positions (
    bucket_id        TEXT         NOT NULL,
    range_start      NUMERIC(39)  NOT NULL,
    range_end        NUMERIC(39)  NOT NULL,
    -- On-chain Position object id (from WriteExecuted.position_id). Positions
    -- are never split, so this is a stable 1:1 key for enrichment.
    object_id        TEXT         NOT NULL,
    recipient        TEXT         NOT NULL,
    updated_at_seq   BIGINT       NOT NULL,
    -- SO-97: provenance denormalized from the minting WriteExecuted so the
    -- GraphQL positions query is a single positions×buckets join.
    premium_received NUMERIC(39)  NOT NULL,
    mm_account_id    TEXT         NOT NULL,
    tx_digest        TEXT         NOT NULL,
    minted_at_ms     BIGINT       NOT NULL,
    PRIMARY KEY (bucket_id, range_start)
);
CREATE INDEX positions_recipient_idx ON positions (recipient);
CREATE INDEX positions_object_id_idx ON positions (object_id);

-- ── Event query support (generalized GraphQL `events` filter) ──────────────
--
-- GIN index backs `payloadContains` (the general matcher) → Postgres
-- `payload @> $json`. `jsonb_path_ops` is the smaller/faster variant that
-- supports exactly the containment operator we use.
CREATE INDEX indexed_events_payload_gin ON indexed_events USING GIN (payload jsonb_path_ops);

-- Per-event participant projection: every address an event touches, fanned
-- out by role. Backs the `participant` filter ("events involving address X
-- in ANY role"), which is also what the per-wallet Activity feed (SO-98)
-- queries. Populated in `Store::stage_batch`; FK cascades on the (never
-- happening) event delete.
CREATE TABLE event_participants (
    sequence  BIGINT NOT NULL REFERENCES indexed_events(sequence) ON DELETE CASCADE,
    address   TEXT   NOT NULL,
    role      TEXT   NOT NULL,
    PRIMARY KEY (sequence, address, role)
);
CREATE INDEX event_participants_address_idx ON event_participants (address);

-- ── PROMOTING A PAYLOAD FIELD TO AN INDEXED COLUMN ────────────────────────
--
-- `payloadContains` (JSONB `@>`) matches any payload field but supports only
-- equality/containment — no ranges, and JSON numbers are stored as strings.
-- When a consumer needs range/sort/fast filtering on a specific field
-- (e.g. `gross_premium`, `nonce`, `strike`), promote it to a real column:
--
--   1. Add a NEW migration (do NOT edit this one once it ships):
--        ALTER TABLE indexed_events ADD COLUMN gross_premium NUMERIC(39);
--        CREATE INDEX indexed_events_gross_premium_idx
--            ON indexed_events (gross_premium);
--      Backfill if needed:
--        UPDATE indexed_events
--           SET gross_premium = (payload->>'gross_premium')::numeric
--         WHERE event_type = 'WriteExecuted';
--   2. Mirror the column in `src/db/schema.rs` (indexed_events table!) and in
--      `IndexedEventRow` / `NewIndexedEventRow` (db/models.rs), populated in
--      `EventBuild::new_event_row`.
--   3. Add a typed field to `EventFilter` (src/graphql.rs) and a clause in the
--      filter compiler (db/repo.rs `compile_event_filter`).
--
-- The raw value still lives in `payload`; the column is just a fast index.
