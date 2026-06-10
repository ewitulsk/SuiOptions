-- price-charting schema (SO-156). Lives on Tiger Data TimescaleDB — one
-- instance per environment, database `tsdb`, extension preinstalled (the
-- guard keeps local timescale/timescaledb containers working too).
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- One row per DeepBook fill on a watched pool. The composite uniqueness on
-- (pool_id, tx_digest, event_index) makes re-ingestion idempotent — restarts
-- and cursor overlaps insert with ON CONFLICT DO NOTHING.
--
-- price is the human quote-per-base ratio (raw / 10^(9 − baseDec + quoteDec),
-- formula verified against a live fill — DEEPBOOK-FINDINGS.md §C); price_raw
-- and base decimals ride along so bars stay self-contained and re-derivable.
CREATE TABLE pool_trades (
    time          TIMESTAMPTZ      NOT NULL,
    pool_id       TEXT             NOT NULL,
    bucket_id     TEXT             NOT NULL,
    price         DOUBLE PRECISION NOT NULL,
    price_raw     NUMERIC          NOT NULL,
    base_qty      NUMERIC          NOT NULL, -- base atomic units
    quote_qty     NUMERIC          NOT NULL, -- quote atomic units
    base_decimals SMALLINT         NOT NULL,
    taker_is_bid  BOOLEAN          NOT NULL,
    tx_digest     TEXT             NOT NULL,
    event_index   BIGINT           NOT NULL,
    UNIQUE (pool_id, tx_digest, event_index)
);
SELECT create_hypertable('pool_trades', 'time');
CREATE INDEX pool_trades_pool_time_idx ON pool_trades (pool_id, time DESC);

-- Single global cursor over DeepBook's OrderFilled stream (the event type is
-- not generic, so one exact-type query covers every pool; rows for unwatched
-- pools are simply skipped). Advanced in the same transaction as the trade
-- batch it covers.
CREATE TABLE watch_cursor (
    id         SMALLINT PRIMARY KEY,
    cursor_tx  TEXT   NOT NULL,
    cursor_ev  BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
