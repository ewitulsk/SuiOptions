-- One covered-call vault per (underlying, settlement, round_ms). On Solana
-- the share mint is a PDA the program creates inside create_vault, so the
-- Sui twin's coin_published crash-recovery state collapses to a single-tx
-- create and the share-coin columns are gone.
--
-- State machine:
--   pending   -- slot claimed, create_vault not yet landed
--   confirmed -- vault created on chain (vault_id = the vault PDA, base58)
--   failed    -- create gave up; outside the active index so a later pass
--                can retry cleanly (salt-idempotent: a landed-but-unrecorded
--                create collides "already in use" and is adopted)
--   retired   -- on-chain vault was paused (decommissioned); frees the slot
--
-- `generation` feeds the vault-PDA salt (see src/salt.rs): retiring a paused
-- vault bumps the retired-row count, so the replacement derives a NEW PDA
-- instead of colliding with (and re-adopting) the paused one. A `failed`
-- create does NOT bump it — its retry reuses the same PDA so a
-- landed-but-unrecorded create resolves by salt collision. The value is
-- stamped at claim time and re-read from the row on crash-resume; it is
-- never recomputed once claimed.
CREATE TABLE scheduler_vaults (
    id                  BIGSERIAL PRIMARY KEY,
    underlying_symbol   TEXT        NOT NULL,
    settlement_symbol   TEXT        NOT NULL,
    round_ms            BIGINT      NOT NULL,
    generation          BIGINT      NOT NULL,
    state               TEXT        NOT NULL,   -- pending|confirmed|failed|retired
    vault_id            TEXT        NULL,        -- vault PDA, base58
    create_signature    TEXT        NULL,
    last_error          TEXT        NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Hard guarantee: at most ONE live vault per (pair, cadence). `failed` and
-- `retired` are excluded so a given-up attempt / decommissioned vault can be
-- replaced by a fresh `pending` insert.
CREATE UNIQUE INDEX scheduler_vaults_active_pair
    ON scheduler_vaults (underlying_symbol, settlement_symbol, round_ms)
    WHERE state IN ('pending', 'confirmed');
