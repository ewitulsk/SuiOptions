-- solana-indexer initial schema.
--
-- Mirrors the Sui indexer's shape (append-only event log + materialised
-- views + singleton progress row) with Solana coordinates:
--   - checkpoint → slot; tx_digest → signature (base58);
--     event_index → global inner-instruction index within the tx.
--   - ids are base58 pubkeys (TEXT).
--   - `indexer_progress.finalized_slot` is the reorg watermark: rows with
--     slot <= finalized_slot are immutable truth; above it they're
--     provisional (ingested at `confirmed`) and may be evicted if their
--     slot is forked away (never observed on mainnet, handled anyway).
--
-- u128-capable quantities are NUMERIC(39) — 2^128 has 39 decimal digits.

CREATE TABLE indexer_progress (
    id             SMALLINT     PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_slot      BIGINT       NOT NULL,
    finalized_slot BIGINT       NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE indexed_events (
    sequence       BIGSERIAL    PRIMARY KEY,
    slot           BIGINT       NOT NULL,
    signature      TEXT         NOT NULL,
    tx_index       BIGINT       NOT NULL,
    inner_ix_index INTEGER      NOT NULL,
    program        TEXT         NOT NULL,
    timestamp_ms   BIGINT       NOT NULL,
    event_type     TEXT         NOT NULL,
    payload        JSONB        NOT NULL,
    -- Idempotency key: replay (fromSlot resume, backfill overlap) upserts
    -- conflict here and are skipped, so view folds never double-apply.
    UNIQUE (signature, inner_ix_index)
);
CREATE INDEX indexed_events_event_type_idx ON indexed_events (event_type);
CREATE INDEX indexed_events_slot_idx ON indexed_events (slot);
CREATE INDEX indexed_events_payload_idx ON indexed_events USING GIN (payload jsonb_path_ops);

-- (event, address, role) edges for the `participant` query filter. Roles
-- are the payload field names (bucket, exerciser, vault, …). CASCADE so a
-- fork eviction of the parent event removes the edges.
CREATE TABLE event_participants (
    sequence BIGINT NOT NULL REFERENCES indexed_events(sequence) ON DELETE CASCADE,
    address  TEXT   NOT NULL,
    role     TEXT   NOT NULL,
    PRIMARY KEY (sequence, address, role)
);
CREATE INDEX event_participants_address_idx ON event_participants (address);

-- MM accounts (options_core account PDAs).
CREATE TABLE accounts (
    account_id      TEXT      PRIMARY KEY,
    owner           TEXT      NOT NULL,
    signing_scheme  SMALLINT  NOT NULL,
    signing_pubkey  BYTEA     NOT NULL DEFAULT '\x',
    updated_at_slot BIGINT    NOT NULL
);

CREATE TABLE account_balances (
    account_id      TEXT         NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    mint            TEXT         NOT NULL,
    balance         NUMERIC(39)  NOT NULL,
    updated_at_slot BIGINT       NOT NULL,
    PRIMARY KEY (account_id, mint)
);

-- Call + put buckets share the table, discriminated by option_kind.
CREATE TABLE buckets (
    bucket_id       TEXT         PRIMARY KEY,
    underlying_mint TEXT         NOT NULL,
    settlement_mint TEXT         NOT NULL,
    -- The option coin: call_mint or put_mint depending on option_kind.
    option_mint     TEXT         NOT NULL,
    option_kind     TEXT         NOT NULL,
    strike          NUMERIC(39)  NOT NULL,
    strike_scale    SMALLINT     NOT NULL DEFAULT 0,
    expiry_ms       BIGINT       NOT NULL,
    total_written   NUMERIC(39)  NOT NULL DEFAULT 0,
    exercise_cursor NUMERIC(39)  NOT NULL DEFAULT 0,
    cleaned         BOOLEAN      NOT NULL DEFAULT false,
    invalidated     BOOLEAN      NOT NULL DEFAULT false,
    updated_at_slot BIGINT       NOT NULL
);

-- Position PDAs. On Solana the position account pubkey is the natural
-- primary key (the Sui table keyed on (bucket, range_start)).
CREATE TABLE positions (
    position_id      TEXT         PRIMARY KEY,
    bucket_id        TEXT         NOT NULL,
    range_start      NUMERIC(39)  NOT NULL,
    range_end        NUMERIC(39)  NOT NULL,
    recipient        TEXT         NOT NULL,
    option_kind      TEXT         NOT NULL,
    -- Provenance denormalized from the minting event: net premium the
    -- writer received (0 for self-collateralized writes) and the quote
    -- signer (null for self-writes).
    premium_received NUMERIC(39)  NOT NULL DEFAULT 0,
    mm_account_id    TEXT,
    signature        TEXT         NOT NULL,
    minted_at_ms     BIGINT       NOT NULL,
    updated_at_slot  BIGINT       NOT NULL,
    UNIQUE (bucket_id, range_start)
);
CREATE INDEX positions_recipient_idx ON positions (recipient);

-- auction_venue auctions (the RFQ/swap successor). status:
-- open | settled | unsold.
CREATE TABLE auctions (
    auction_id        TEXT         PRIMARY KEY,
    mode              TEXT         NOT NULL,
    -- Null for pure swaps (on-chain Pubkey::default()).
    bucket_id         TEXT,
    creator           TEXT         NOT NULL,
    escrow_mint       TEXT         NOT NULL,
    bid_mint          TEXT         NOT NULL,
    amount            NUMERIC(39)  NOT NULL,
    notional          NUMERIC(39)  NOT NULL,
    reserve_bid       NUMERIC(39)  NOT NULL,
    deadline_ms       BIGINT       NOT NULL,
    max_deadline_ms   BIGINT       NOT NULL,
    min_increment_bps BIGINT       NOT NULL,
    settle_authority  TEXT,
    best_bid          NUMERIC(39),
    best_bidder       TEXT,
    status            TEXT         NOT NULL DEFAULT 'open',
    winner            TEXT,
    token_recipient   TEXT,
    position_id       TEXT,
    gross_bid         NUMERIC(39),
    fee               NUMERIC(39),
    net_proceeds      NUMERIC(39),
    bid_refunded      BOOLEAN,
    updated_at_slot   BIGINT       NOT NULL
);
CREATE INDEX auctions_status_idx ON auctions (status);
CREATE INDEX auctions_creator_idx ON auctions (creator);

-- Append-only bid history, keyed by the bid event's log sequence.
CREATE TABLE auction_bids (
    auction_id      TEXT         NOT NULL,
    sequence        BIGINT       NOT NULL,
    bidder          TEXT         NOT NULL,
    token_recipient TEXT         NOT NULL,
    bid             NUMERIC(39)  NOT NULL,
    previous_bid    NUMERIC(39)  NOT NULL,
    deadline_ms     BIGINT       NOT NULL,
    PRIMARY KEY (auction_id, sequence)
);

CREATE TABLE vaults (
    vault_id                 TEXT         PRIMARY KEY,
    underlying_mint          TEXT         NOT NULL,
    settlement_mint          TEXT         NOT NULL,
    share_mint               TEXT         NOT NULL,
    round                    BIGINT       NOT NULL DEFAULT 0,
    current_bucket           TEXT,
    latest_pps               NUMERIC(39),
    total_shares             NUMERIC(39)  NOT NULL DEFAULT 0,
    pending_deposits         NUMERIC(39)  NOT NULL DEFAULT 0,
    deposits_paused          BOOLEAN      NOT NULL DEFAULT false,
    mgmt_fee_bps_annual      BIGINT,
    perf_fee_bps             BIGINT,
    round_ms                 BIGINT,
    selling_window_ms        BIGINT,
    min_strike_bps_over_spot BIGINT,
    max_strike_bps_over_spot BIGINT,
    updated_at_slot          BIGINT       NOT NULL
);

CREATE TABLE vault_rounds (
    vault_id          TEXT         NOT NULL,
    round             BIGINT       NOT NULL,
    bucket_id         TEXT,
    strike            NUMERIC(39),
    strike_scale      SMALLINT,
    expiry_ms         BIGINT,
    selling_ends_ms   BIGINT,
    spot              NUMERIC(39),
    spot_scale        SMALLINT,
    pps               NUMERIC(39),
    aum               NUMERIC(39),
    shares            NUMERIC(39),
    premium_collected NUMERIC(39),
    mgmt_fee          NUMERIC(39),
    perf_fee          NUMERIC(39),
    finalized_at_ms   BIGINT,
    updated_at_slot   BIGINT       NOT NULL,
    PRIMARY KEY (vault_id, round)
);

-- Per-(vault, owner, round, kind) receipt aggregates. kind:
-- deposit | withdraw. `amount` is queued underlying (deposits) or
-- escrowed shares (withdrawals); `settled` what's been claimed/completed.
CREATE TABLE vault_receipts (
    vault_id        TEXT         NOT NULL,
    owner           TEXT         NOT NULL,
    round           BIGINT       NOT NULL,
    kind            TEXT         NOT NULL,
    amount          NUMERIC(39)  NOT NULL DEFAULT 0,
    settled         NUMERIC(39)  NOT NULL DEFAULT 0,
    updated_at_slot BIGINT       NOT NULL,
    PRIMARY KEY (vault_id, owner, round, kind)
);
