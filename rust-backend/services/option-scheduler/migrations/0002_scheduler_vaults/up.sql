-- One covered-call vault per (underlying, settlement) pair. The scheduler
-- ensures a vault exists for every configured pair; this table is the
-- crash-safe idempotency record so a wiped indexer or a restart mid-create
-- can never spawn a duplicate vault.
--
-- State machine:
--   pending        -- slot claimed, share coin not yet published
--   coin_published -- VShare coin package published + TreasuryCap harvested;
--                     create_vault not yet landed. The crash-recovery hinge:
--                     a row here resumes from create_vault reusing the stored
--                     cap, so we never publish a second (orphan) share coin.
--   confirmed      -- vault created on chain (vault_id recorded)
--   failed         -- create gave up; outside the active index so a later
--                     pass can retry cleanly
CREATE TABLE scheduler_vaults (
    id                  BIGSERIAL PRIMARY KEY,
    underlying_symbol   TEXT        NOT NULL,
    settlement_symbol   TEXT        NOT NULL,
    state               TEXT        NOT NULL,   -- pending|coin_published|confirmed|failed
    share_coin_package  TEXT        NULL,        -- published VShare coin package id
    share_coin_type     TEXT        NULL,        -- canonical VShare type tag
    share_cap_id        TEXT        NULL,        -- TreasuryCap<VShare> object id
    vault_id            TEXT        NULL,        -- created Vault object id
    publish_digest      TEXT        NULL,
    create_digest       TEXT        NULL,
    last_error          TEXT        NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Hard guarantee: at most ONE live vault per pair. `failed` is excluded so a
-- given-up attempt can be retried by a fresh `pending` insert.
CREATE UNIQUE INDEX scheduler_vaults_active_pair
    ON scheduler_vaults (underlying_symbol, settlement_symbol)
    WHERE state IN ('pending', 'coin_published', 'confirmed');
