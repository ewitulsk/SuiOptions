-- C3/D2 (vault-implementation-guide doc 05 §1.2): materialised views for
-- the on-chain RFQ auctions and the covered-call vaults, fed from the 18
-- new event types. Same write-through pattern as buckets/accounts: the
-- in-memory store applies events and stages absolute-value rows.

CREATE TABLE rfqs (
    rfq_id          TEXT         PRIMARY KEY,
    bucket_id       TEXT         NOT NULL,
    origin          TEXT         NOT NULL,            -- vault id or seller-as-id
    amount          NUMERIC(39)  NOT NULL,
    reserve_premium NUMERIC(39)  NOT NULL,
    deadline_ms     BIGINT       NOT NULL,            -- updated on anti-snipe extensions
    best_premium    NUMERIC(39),
    best_bidder     TEXT,
    status          TEXT         NOT NULL,            -- open | settled | expired_unsold
    winner          TEXT,
    net_premium     NUMERIC(39),
    position_id     TEXT,
    updated_at_seq  BIGINT       NOT NULL
);
CREATE INDEX rfqs_status_idx ON rfqs (status);
CREATE INDEX rfqs_origin_idx ON rfqs (origin);

-- Append-only bid history (one row per RfqBid event).
CREATE TABLE rfq_bids (
    rfq_id          TEXT         NOT NULL,
    sequence        BIGINT       NOT NULL REFERENCES indexed_events(sequence) ON DELETE CASCADE,
    bidder          TEXT         NOT NULL,
    call_recipient  TEXT         NOT NULL,
    premium         NUMERIC(39)  NOT NULL,
    PRIMARY KEY (rfq_id, sequence)
);

CREATE TABLE vaults (
    vault_id         TEXT         PRIMARY KEY,
    underlying_type  TEXT         NOT NULL,
    settlement_type  TEXT         NOT NULL,
    share_type       TEXT         NOT NULL,
    -- Current round: last finalized round + 1 (0 = pre-genesis settling).
    round            BIGINT       NOT NULL,
    current_bucket   TEXT,
    latest_pps       NUMERIC(39),
    -- Share supply after the last finalize (burns and mints applied).
    total_shares     NUMERIC(39)  NOT NULL,
    -- Deposits queued for the next round (running sum of VaultDeposit −
    -- InstantWithdraw − deposits_processed at finalize).
    pending_deposits NUMERIC(39)  NOT NULL,
    deposits_paused  BOOLEAN      NOT NULL DEFAULT false,
    updated_at_seq   BIGINT       NOT NULL
);

-- Per-round track record: assembled from VaultBucketSelected (strike /
-- expiry), VaultFeesCharged, and VaultRoundFinalized (pps / aum / premium).
CREATE TABLE vault_rounds (
    vault_id          TEXT         NOT NULL,
    round             BIGINT       NOT NULL,
    bucket_id         TEXT,
    strike            NUMERIC(39),
    strike_scale      SMALLINT,
    expiry_ms         BIGINT,
    pps               NUMERIC(39),
    aum               NUMERIC(39),
    shares            NUMERIC(39),
    premium_collected NUMERIC(39),
    mgmt_fee          NUMERIC(39),
    perf_fee          NUMERIC(39),
    finalized_at_ms   BIGINT,
    updated_at_seq    BIGINT       NOT NULL,
    PRIMARY KEY (vault_id, round)
);

-- "My deposits / withdrawals" aggregates. The receipt events don't carry
-- the receipt object ids, so rows aggregate per (vault, owner, round,
-- kind) instead of per object (deviation from the guide sketch, which
-- assumed ids in the events). `amount` is queued underlying for deposits
-- and escrowed shares for withdrawals; `settled` is what has been
-- claimed / completed / instant-withdrawn so far. Claimability is derived
-- by the API from the vault's current round.
CREATE TABLE vault_user_receipts (
    vault_id        TEXT         NOT NULL,
    owner           TEXT         NOT NULL,
    round           BIGINT       NOT NULL,
    kind            TEXT         NOT NULL,             -- deposit | withdraw
    amount          NUMERIC(39)  NOT NULL,
    settled         NUMERIC(39)  NOT NULL,
    updated_at_seq  BIGINT       NOT NULL,
    PRIMARY KEY (vault_id, owner, round, kind)
);
CREATE INDEX vault_user_receipts_owner_idx ON vault_user_receipts (owner);
