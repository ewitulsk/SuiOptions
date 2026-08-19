-- SO-418: trading-vault v2 (vault_v2 package). Per-tranche capital state on
-- the vault view (fed by TvCapitalSynced + the tranche-tagged deposit /
-- withdraw events), the junior generational-reset proposal, the terminal
-- settlement pool, per-lane queue cursors, and a new `vault_positions`
-- lifecycle table for the VaultPosition NFTs (mint / split / merge /
-- queue / settle / burn). Ownership is NOT indexed — transfers emit no
-- events; api-service resolves owners JIT from chain.

ALTER TABLE trading_vaults
    -- Immutable capital structure + terms (TvVaultCreated).
    ADD COLUMN structure_code            SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN senior_hurdle_bps_annual  BIGINT   NOT NULL DEFAULT 0,
    ADD COLUMN target_junior_bps         BIGINT   NOT NULL DEFAULT 0,
    ADD COLUMN maintenance_junior_bps    BIGINT   NOT NULL DEFAULT 0,
    ADD COLUMN upside_code               SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN residual_participation_bps BIGINT  NOT NULL DEFAULT 0,
    ADD COLUMN total_return_cap_bps      BIGINT   NOT NULL DEFAULT 0,
    ADD COLUMN terms_version             BIGINT   NOT NULL DEFAULT 0,
    -- 0x-prefixed hex; null for pre-v2 rows.
    ADD COLUMN spec_hash                 TEXT,
    -- Tranche book (TvCapitalSynced is authoritative; deposits/withdraws
    -- carry post-event tranche supplies too). Untranched supply lives in
    -- junior_shares, mirroring capital.move.
    ADD COLUMN senior_shares             NUMERIC(39) NOT NULL DEFAULT 0,
    ADD COLUMN junior_shares             NUMERIC(39) NOT NULL DEFAULT 0,
    ADD COLUMN senior_claim              NUMERIC(39) NOT NULL DEFAULT 0,
    -- Sum of senior deposit values minus withdrawn/settled senior basis
    -- (additive — same replay caveat as pending_withdrawals).
    ADD COLUMN senior_principal_basis    NUMERIC(39) NOT NULL DEFAULT 0,
    ADD COLUMN senior_nav                NUMERIC(39),
    ADD COLUMN junior_nav                NUMERIC(39),
    -- Observed per-tranche pps (1e12-scaled, SHARE_OFFSET-adjusted), from
    -- tranche-tagged deposits/fulfils + capital syncs. Untranched vaults
    -- keep using latest_pps_e12.
    ADD COLUMN latest_senior_pps_e12     NUMERIC(39),
    ADD COLUMN latest_junior_pps_e12     NUMERIC(39),
    -- Risk state machine: 0=Healthy 1=CoverageBreach 2=Impaired
    -- 3=ResetPending.
    ADD COLUMN risk_state                SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN curator_commitment_breached BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN impaired_since_ms         BIGINT,
    ADD COLUMN active_junior_generation  BIGINT NOT NULL DEFAULT 0,
    -- Open junior-reset proposal (null columns = no proposal).
    ADD COLUMN reset_old_generation      BIGINT,
    ADD COLUMN reset_proposed_at_ms      BIGINT,
    ADD COLUMN reset_executable_at_ms    BIGINT,
    ADD COLUMN reset_recorded_nav        NUMERIC(39),
    ADD COLUMN reset_recorded_senior_claim NUMERIC(39),
    ADD COLUMN reset_recorded_required_deposit BIGINT,
    -- Terminal settlement pool (TvSettlementSnapshot freezes it;
    -- TvSettlementRedeemed accumulates entitlement draws).
    ADD COLUMN settled                   BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN settlement_final_nav      NUMERIC(39),
    ADD COLUMN senior_pool               BIGINT,
    ADD COLUMN senior_supply             NUMERIC(39),
    ADD COLUMN junior_pool               BIGINT,
    ADD COLUMN junior_supply             NUMERIC(39),
    ADD COLUMN settlement_snapshot_at_ms BIGINT,
    ADD COLUMN settlement_redeemed       NUMERIC(39) NOT NULL DEFAULT 0,
    -- Per-lane queue cursors observed from the event stream: tail = highest
    -- requested global_seq + 1; head = highest fulfilled/settled
    -- global_seq + 1 on that lane.
    ADD COLUMN senior_lane_head          BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN senior_lane_tail          BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN junior_lane_head          BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN junior_lane_tail          BIGINT NOT NULL DEFAULT 0;

-- VaultPosition NFT lifecycle + lineage. One row per position object id,
-- status: live | queued | consumed | settled | burned. Rows never delete —
-- consumed/settled/burned stay for history and supply-conservation checks.
CREATE TABLE vault_positions (
    position_id        TEXT        PRIMARY KEY,
    vault_id           TEXT        NOT NULL,
    -- 0=Untranched 1=Senior 2=Junior.
    tranche            SMALLINT    NOT NULL,
    capital_generation BIGINT      NOT NULL,
    shares             NUMERIC(39) NOT NULL,
    cost_basis         NUMERIC(39) NOT NULL,
    locked_until_ms    BIGINT      NOT NULL,
    status             TEXT        NOT NULL,
    -- Split lineage: the parent this position was carved out of.
    parent_position_id TEXT,
    -- Merge lineage: the kept position this one was folded into.
    merged_into        TEXT,
    -- Set while status = queued: the withdraw request's global_seq (the
    -- fulfil event carries no position id, so this is the join key).
    queued_global_seq  BIGINT,
    created_at_ms      BIGINT      NOT NULL,
    updated_at_ms      BIGINT      NOT NULL,
    updated_at_seq     BIGINT      NOT NULL
);

CREATE INDEX vault_positions_by_vault ON vault_positions (vault_id);
