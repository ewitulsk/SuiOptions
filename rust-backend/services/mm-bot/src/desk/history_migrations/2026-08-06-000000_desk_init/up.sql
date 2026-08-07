-- mm-bot desk history (SO-349). Lives on the SAME Tiger Data TimescaleDB
-- instance as price-charting (per-env, database `tsdb`) — tables are
-- desk_-prefixed and the migration version is date-stamped so the two
-- services' embedded migrations coexist in the shared
-- __diesel_schema_migrations table without version collisions.
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- One row per recorder tick (default 60s): headline exposure, soft-limit
-- utilizations, concentration, and the risk gates.
CREATE TABLE desk_snapshots (
    time                  TIMESTAMPTZ      NOT NULL,
    nav                   DOUBLE PRECISION NOT NULL,
    deployed              DOUBLE PRECISION NOT NULL,
    reserved              DOUBLE PRECISION NOT NULL,
    net_vega_per_volpt    DOUBLE PRECISION NOT NULL,
    theta_cost_per_day    DOUBLE PRECISION NOT NULL,
    premium_util          DOUBLE PRECISION NOT NULL,
    vega_util             DOUBLE PRECISION NOT NULL,
    theta_util            DOUBLE PRECISION NOT NULL,
    premium_lt90          DOUBLE PRECISION NOT NULL,
    premium_90_110        DOUBLE PRECISION NOT NULL,
    premium_gt110         DOUBLE PRECISION NOT NULL,
    naked_units           BIGINT           NOT NULL,
    funding_rate_annual   DOUBLE PRECISION NOT NULL,
    kill_switch           BOOLEAN          NOT NULL,
    stress_blocked        BOOLEAN          NOT NULL,
    -- NULL until the first nightly stress suite has run this boot.
    worst_stress_drawdown DOUBLE PRECISION
);
SELECT create_hypertable('desk_snapshots', 'time');

-- Per-underlying delta-vs-hedge sample (the delta-band chart).
CREATE TABLE desk_symbol_samples (
    time              TIMESTAMPTZ      NOT NULL,
    symbol            TEXT             NOT NULL,
    spot              DOUBLE PRECISION,
    book_delta_units  DOUBLE PRECISION NOT NULL,
    hedge_short_units DOUBLE PRECISION NOT NULL,
    net_delta_units   DOUBLE PRECISION NOT NULL,
    band_units        DOUBLE PRECISION
);
SELECT create_hypertable('desk_symbol_samples', 'time');
CREATE INDEX desk_symbol_samples_sym_idx ON desk_symbol_samples (symbol, time DESC);

-- Per hedge-venue-instance sample (short/funding/margin/realized P&L).
CREATE TABLE desk_venue_samples (
    time            TIMESTAMPTZ      NOT NULL,
    venue           TEXT             NOT NULL,
    symbol          TEXT             NOT NULL,
    short_units     DOUBLE PRECISION NOT NULL,
    funding_annual  DOUBLE PRECISION NOT NULL,
    margin_headroom DOUBLE PRECISION NOT NULL,
    notional        DOUBLE PRECISION NOT NULL,
    realized_pnl    DOUBLE PRECISION NOT NULL
);
SELECT create_hypertable('desk_venue_samples', 'time');
CREATE INDEX desk_venue_samples_vs_idx ON desk_venue_samples (venue, symbol, time DESC);

-- Net greeks + deployed premium per expiry bucket.
CREATE TABLE desk_expiry_samples (
    time          TIMESTAMPTZ      NOT NULL,
    expiry_ms     BIGINT           NOT NULL,
    premium       DOUBLE PRECISION NOT NULL,
    delta_units   DOUBLE PRECISION NOT NULL,
    gamma_units   DOUBLE PRECISION NOT NULL,
    vega          DOUBLE PRECISION NOT NULL,
    theta_per_day DOUBLE PRECISION NOT NULL
);
SELECT create_hypertable('desk_expiry_samples', 'time');
CREATE INDEX desk_expiry_samples_exp_idx ON desk_expiry_samples (expiry_ms, time DESC);

-- P&L attribution mirrored from the desk's append-only JSONL ledger
-- (spread/scalp/theta/funding). Exactly-once: the byte offset in
-- desk_pnl_ingest advances in the same transaction as the batch it
-- covers, so a crash between insert and offset persist replays nothing.
CREATE TABLE desk_pnl_lines (
    time   TIMESTAMPTZ      NOT NULL,
    ts_ms  BIGINT           NOT NULL,
    line   TEXT             NOT NULL,
    amount DOUBLE PRECISION NOT NULL,
    note   TEXT             NOT NULL
);
SELECT create_hypertable('desk_pnl_lines', 'time');
CREATE INDEX desk_pnl_lines_line_idx ON desk_pnl_lines (line, time DESC);

CREATE TABLE desk_pnl_ingest (
    id          SMALLINT PRIMARY KEY,
    byte_offset BIGINT NOT NULL
);
