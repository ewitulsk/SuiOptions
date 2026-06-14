CREATE TABLE vault_predictions (
    vault_id    TEXT             NOT NULL,
    kind        TEXT             NOT NULL,           -- 'current' | 'forecast'
    horizon     INT              NOT NULL,           -- 0 = current round, 1..K ahead
    t_ms        BIGINT           NOT NULL,
    apy         DOUBLE PRECISION NOT NULL,
    confidence  DOUBLE PRECISION NOT NULL,
    computed_at TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    PRIMARY KEY (vault_id, kind, horizon)
);

CREATE INDEX vault_predictions_vault_idx ON vault_predictions (vault_id);
