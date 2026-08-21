-- leaderboard schema, v1.
--
-- Accounts are identity-agnostic: any (identity_type, identifier) pair maps
-- to exactly one account, and identities can be linked/merged later without
-- touching the points ledger. merged_into is audit-only — a merge repoints
-- every row so queries never chase chains.

CREATE TABLE accounts (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    merged_into BIGINT REFERENCES accounts(id)
);

CREATE TABLE account_identities (
    identity_type TEXT        NOT NULL CHECK (identity_type IN ('wallet', 'twitter', 'discord')),
    identifier    TEXT        NOT NULL,
    account_id    BIGINT      NOT NULL REFERENCES accounts(id),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One identity belongs to exactly one account; this is also the lookup
    -- index for every public read path.
    PRIMARY KEY (identity_type, identifier)
);

CREATE INDEX account_identities_account_idx ON account_identities(account_id);

-- The ledger. delta is signed (negative = removal). idempotency_key is the
-- dedupe authority for at-least-once delivery from the event ingestor
-- ("{tx_digest}:{event_seq}:{rule_id}") and is globally unique so keys
-- survive merges. source discriminates where points came from
-- ('rule:42' | 'admin:manual').
CREATE TABLE points_entries (
    id              BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id      BIGINT      NOT NULL REFERENCES accounts(id),
    delta           BIGINT      NOT NULL,
    source          TEXT        NOT NULL,
    event_type      TEXT,
    idempotency_key TEXT        UNIQUE,
    occurred_at     TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX points_entries_occurred_idx       ON points_entries(occurred_at);
CREATE INDEX points_entries_account_time_idx   ON points_entries(account_id, occurred_at);
CREATE INDEX points_entries_source_time_idx    ON points_entries(source, occurred_at);
CREATE INDEX points_entries_event_type_time_idx ON points_entries(event_type, occurred_at);

-- Cached all-time totals, maintained in the same transaction as every
-- insert. Windowed ranks re-aggregate the ledger instead (fine at launch
-- volume; rollup tables are the escape hatch).
CREATE TABLE account_totals (
    account_id BIGINT      PRIMARY KEY REFERENCES accounts(id),
    total      BIGINT      NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX account_totals_total_idx ON account_totals(total DESC);

-- Human labels for the public source filter dropdown, upserted from the
-- optional source_label on internal writes.
CREATE TABLE sources (
    source     TEXT        PRIMARY KEY,
    event_type TEXT,
    label      TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Merge audit trail.
CREATE TABLE account_merges (
    winner_account_id BIGINT      NOT NULL REFERENCES accounts(id),
    loser_account_id  BIGINT      NOT NULL REFERENCES accounts(id),
    merged_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (winner_account_id, loser_account_id)
);
