-- RFQ outcome funnel (SO-425, backtester PR A — doc 08 §3.1). One row per
-- signed-quote opportunity: every WS RFQ decision and every vault-funded
-- auction bid, carried to exactly one terminal outcome. `outcome =
-- 'quoted'` is the only non-terminal state: the recorder sweeps it to
-- 'expired' after the quote TTL, and the fill poller upgrades it (or a
-- swept 'expired') to 'filled' when the write lands on chain — a
-- detected fill is ground truth and always wins.
CREATE TABLE desk_rfq_outcomes (
    -- Request received (WS) / bid placed (auction).
    time                TIMESTAMPTZ      NOT NULL,
    -- WS: service request id. Auction: BidTicket id hex (one row per
    -- bid — rebids after an outbid are new rows).
    request_id          TEXT             NOT NULL,
    source              TEXT             NOT NULL, -- 'ws' | 'auction'
    -- Auction rows: the RFQ/auction object id the ticket bid on.
    auction_id          TEXT,
    symbol              TEXT,
    option_type         TEXT             NOT NULL, -- 'call' | 'put'
    side                TEXT             NOT NULL, -- 'writer' | 'trader'
    strike              DOUBLE PRECISION NOT NULL,
    expiry_ms           BIGINT           NOT NULL,
    size_units          BIGINT           NOT NULL,
    spot_at_request     DOUBLE PRECISION,
    -- Model fair TOTAL premium and surface vol at decision time,
    -- settlement raw / annualized. NULL when declined before pricing.
    model_fair          DOUBLE PRECISION,
    surface_vol         DOUBLE PRECISION,
    quoted_premium      BIGINT,
    valid_until_ms      BIGINT,
    -- WS quotes: the signed quote nonce (the WriteExecuted join key).
    nonce               BIGINT,
    response_latency_ms DOUBLE PRECISION,
    outcome             TEXT             NOT NULL,
    outcome_at          TIMESTAMPTZ,
    -- Decline/refusal reason, or the terminal transition's cause.
    reason              TEXT,
    -- Indexer sequence of the fill event that closed this row.
    fill_sequence       BIGINT
);
SELECT create_hypertable('desk_rfq_outcomes', 'time');

-- The expiry sweep scans only live quotes.
CREATE INDEX desk_rfq_outcomes_pending
    ON desk_rfq_outcomes (valid_until_ms) WHERE outcome = 'quoted';
-- Fill upgrades join by nonce (WS) or request_id (auction ticket).
CREATE INDEX desk_rfq_outcomes_nonce ON desk_rfq_outcomes (nonce)
    WHERE nonce IS NOT NULL;
CREATE INDEX desk_rfq_outcomes_request ON desk_rfq_outcomes (request_id);
