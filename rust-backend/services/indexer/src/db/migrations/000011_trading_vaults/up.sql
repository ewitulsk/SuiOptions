-- SO-282/SO-287: materialised views for the curated trading vaults, fed
-- from the Tv* event stream. Same write-through pattern as vaults: the
-- in-memory store applies events and stages absolute-value rows.

CREATE TABLE trading_vaults (
    vault_id            TEXT         PRIMARY KEY,
    deposit_asset       TEXT         NOT NULL,
    creator             TEXT         NOT NULL,
    -- Current curator wallet (updated on TvCuratorRotated).
    curator             TEXT         NOT NULL,
    curator_cap_id      TEXT         NOT NULL,
    state               TEXT         NOT NULL,          -- open | closing | closed
    lockup_ms           BIGINT       NOT NULL,
    curator_fee_bps     BIGINT       NOT NULL,
    rotation_authority  SMALLINT     NOT NULL,
    max_positions       BIGINT       NOT NULL,
    unwind_grace_ms     BIGINT       NOT NULL,
    deposits_paused     BOOLEAN      NOT NULL DEFAULT false,
    mm_release_enabled  BOOLEAN      NOT NULL DEFAULT false,
    total_shares        NUMERIC(39)  NOT NULL,
    position_count      BIGINT       NOT NULL,
    pending_withdrawals BIGINT       NOT NULL,
    -- Observed deposit-asset-per-share price (1e12-scaled), inferred from
    -- the latest TvDeposited / TvWithdrawFulfilled.
    latest_pps_e12      NUMERIC(39),
    updated_at_seq      BIGINT       NOT NULL,
    updated_at_ms       BIGINT       NOT NULL
);

-- Adapter positions per vault. Removed positions stay with active=false so
-- "past positions" render.
CREATE TABLE trading_vault_positions (
    vault_id       TEXT     NOT NULL,
    position_id    TEXT     NOT NULL,
    adapter        TEXT     NOT NULL,
    active         BOOLEAN  NOT NULL,
    stored_at_ms   BIGINT   NOT NULL,
    removed_at_ms  BIGINT,
    updated_at_seq BIGINT   NOT NULL,
    PRIMARY KEY (vault_id, position_id)
);
