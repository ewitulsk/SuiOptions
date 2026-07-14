-- solana-price-charting schema. Lives on the Tiger Data TimescaleDB
-- instance in its own `solana_tsdb` database (extension preinstalled; the
-- guard keeps local timescale/timescaledb containers working too).
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- One row per fill on a watched pool. No Solana order-book ingestion exists
-- yet — this table stays empty until a venue integration lands; the schema
-- is the Sui twin's with the idempotency column renamed `tx_digest` →
-- `signature` (Solana tx signature; fresh DB, no compat burden). The
-- composite uniqueness on (pool_id, signature, event_index, time) makes
-- re-ingestion idempotent — restarts and cursor overlaps insert with
-- ON CONFLICT DO NOTHING. `time` is in the key because TimescaleDB requires
-- every unique constraint on a hypertable to include the partitioning
-- column; it's functionally determined by (signature, event_index) so
-- uniqueness is unchanged.
--
-- price is the human quote-per-base ratio; price_raw and base decimals ride
-- along so bars stay self-contained and re-derivable.
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
    signature     TEXT             NOT NULL,
    event_index   BIGINT           NOT NULL,
    UNIQUE (pool_id, signature, event_index, time)
);
SELECT create_hypertable('pool_trades', 'time');
CREATE INDEX pool_trades_pool_time_idx ON pool_trades (pool_id, time DESC);

-- Single global cursor for the future fill-ingestion task, advanced in the
-- same transaction as the trade batch it covers. Unwritten until ingestion
-- exists.
CREATE TABLE watch_cursor (
    id         SMALLINT PRIMARY KEY,
    cursor_tx  TEXT   NOT NULL,
    cursor_ev  BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
