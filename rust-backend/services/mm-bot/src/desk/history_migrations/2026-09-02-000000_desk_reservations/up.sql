-- Durable quote reservations (SO-444, doc 08 §4.6). One row per signed
-- quote / legacy bid, keyed by the request id, carried through the
-- explicit lifecycle quoted → accepted | reverted | expired | filled.
-- 'quoted' and 'accepted' are LIVE (they hold premium capacity); the
-- desk mirrors those in memory and re-installs them at boot after
-- reconciling against chain fills. A detected fill is ground truth: it
-- upgrades any row, including one already swept to 'expired'.
CREATE TABLE desk_reservations (
    request_id     TEXT             PRIMARY KEY,
    -- Signed quote nonce (the (Put)WriteExecuted join key); NULL for
    -- legacy/auction reservations.
    nonce          BIGINT,
    -- Premium reserved, settlement raw.
    amount         BIGINT           NOT NULL,
    is_put         BOOLEAN          NOT NULL,
    -- Option expiry (per-expiry capacity numerator).
    expiry_ms      BIGINT           NOT NULL,
    -- Strike cash (calls) / underlying value (puts) and hedge notional
    -- the fill would need — the exercise / margin capacity numerators.
    exercise_cash  DOUBLE PRECISION NOT NULL,
    hedge_notional DOUBLE PRECISION NOT NULL,
    quoted_at_ms   BIGINT           NOT NULL,
    -- Reservation TTL: quote valid_until + fill-detection grace.
    expires_ms     BIGINT           NOT NULL,
    state          TEXT             NOT NULL,
    state_at_ms    BIGINT           NOT NULL,
    updated_at     TIMESTAMPTZ      NOT NULL DEFAULT now()
);

-- Boot reload scans only live rows.
CREATE INDEX desk_reservations_live
    ON desk_reservations (expires_ms) WHERE state IN ('quoted', 'accepted');
CREATE INDEX desk_reservations_nonce ON desk_reservations (nonce)
    WHERE nonce IS NOT NULL;
