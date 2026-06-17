-- Vault APY time-series (SO-folding derived-metric-worker into price-charting).
-- These two hypertables replace the derived-metric-worker's single
-- `vault_predictions` snapshot table: instead of upserting the latest
-- prediction per (vault, kind, horizon), the apy sampler APPENDS a row each
-- tick, so predicted-APY history is retained over time alongside realized.

-- Predicted (forward-looking) APY. One row per (vault, kind, horizon) PER
-- sampler tick — `time` is the compute time, so the series is the history of
-- what the model predicted as inputs (spot / vol / RFQ premium) moved.
--   kind:    'current' (Tier 1, this round's live RFQ premium) | 'forecast'
--            (Tier 2, Black-Scholes premium for a future round).
--   horizon: 0 = current round; 1..=K = rounds ahead.
--   t_ms:    the expiry the point lands on (the x-value on the APY chart).
CREATE TABLE vault_predicted_apy (
    time        TIMESTAMPTZ      NOT NULL,
    vault_id    TEXT             NOT NULL,
    kind        TEXT             NOT NULL,
    horizon     INT              NOT NULL,
    t_ms        BIGINT           NOT NULL,
    apy         DOUBLE PRECISION NOT NULL,
    confidence  DOUBLE PRECISION NOT NULL,
    UNIQUE (vault_id, kind, horizon, time)
);
SELECT create_hypertable('vault_predicted_apy', 'time');
CREATE INDEX vault_predicted_apy_vault_time_idx
    ON vault_predicted_apy (vault_id, time DESC);

-- Realized APY: annualized pps growth per FINALIZED round, sourced from the
-- indexer's `vaultApy` query (the indexer is the source of the formula; we
-- only persist it so it becomes a queryable time-series rather than an
-- on-read recompute). A round finalizes exactly once and its pps/finalize-time
-- never change, so (vault_id, round) is the natural idempotency key and the
-- sampler inserts ON CONFLICT DO NOTHING. `time` is the round's finalize time.
CREATE TABLE vault_realized_apy (
    time      TIMESTAMPTZ      NOT NULL,
    vault_id  TEXT             NOT NULL,
    round     BIGINT           NOT NULL,
    apy       DOUBLE PRECISION NOT NULL,
    UNIQUE (vault_id, round, time)
);
SELECT create_hypertable('vault_realized_apy', 'time');
CREATE INDEX vault_realized_apy_vault_time_idx
    ON vault_realized_apy (vault_id, time DESC);

-- No automatic retention policy: the predicted series is low-volume (a handful
-- of points per vault per 2-minute tick) and the team keeps Timescale eviction
-- OFF by convention. To cap growth later, enable a native policy, e.g.:
--   SELECT add_retention_policy('vault_predicted_apy', INTERVAL '90 days');
