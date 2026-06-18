-- Predicted APY is now net of expected assignment (SO): each point carries a
-- low/high range, the per-round assignment probability, and the single-round
-- downside if assigned. Defaults keep pre-existing rows valid; the sampler
-- writes real values from the next tick on.
ALTER TABLE vault_predicted_apy
    ADD COLUMN apy_low              DOUBLE PRECISION NOT NULL DEFAULT 0,
    ADD COLUMN apy_high             DOUBLE PRECISION NOT NULL DEFAULT 0,
    ADD COLUMN assignment_prob      DOUBLE PRECISION NOT NULL DEFAULT 0,
    ADD COLUMN downside_round_yield DOUBLE PRECISION NOT NULL DEFAULT 0;
