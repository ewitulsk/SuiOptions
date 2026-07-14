-- Order-book midpoint samples. One row per watched pool per sampler tick
-- where BOTH sides of the book were present (no mid exists otherwise).
-- Prices are display units (quote per base), same scale convention as
-- pool_trades.price. `time` is in the unique key for the hypertable
-- partitioning-column rule, like pool_trades. Empty until a Solana
-- order-book source lands (no mid sampler is ported yet).
CREATE TABLE pool_mids (
    time      TIMESTAMPTZ      NOT NULL,
    pool_id   TEXT             NOT NULL,
    bucket_id TEXT             NOT NULL,
    best_bid  DOUBLE PRECISION NOT NULL,
    best_ask  DOUBLE PRECISION NOT NULL,
    mid       DOUBLE PRECISION NOT NULL,
    UNIQUE (pool_id, time)
);
SELECT create_hypertable('pool_mids', 'time');
CREATE INDEX pool_mids_pool_time_idx ON pool_mids (pool_id, time DESC);
