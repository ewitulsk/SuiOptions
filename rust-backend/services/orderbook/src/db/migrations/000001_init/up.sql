-- Hybrid exchange orderbook schema (spec §5.1 `store`).
--
-- Amounts are BIGINT: Sui amounts are u64 but every practical coin amount
-- fits i64; intake rejects orders whose amounts exceed i64::MAX.
-- Tables are exchange_-prefixed so the service can share a database with
-- other product schemas without collision.

CREATE TABLE exchange_markets (
    registry_id     TEXT PRIMARY KEY,
    symbol          TEXT NOT NULL,
    base            TEXT NOT NULL,
    quote           TEXT NOT NULL,
    tick_size       BIGINT NOT NULL,
    min_size        BIGINT NOT NULL,
    lot_size        BIGINT NOT NULL,
    current_fee_bps BIGINT NOT NULL
);

-- Signed orders. status: OPEN | FILLED | CANCELLED | PRUNED | EXPIRED.
-- BCS bytes + digest are computed once at intake and cached (spec §8).
CREATE TABLE exchange_orders (
    digest          TEXT PRIMARY KEY,
    registry_id     TEXT NOT NULL REFERENCES exchange_markets (registry_id),
    maker           TEXT NOT NULL,
    manager_id      TEXT NOT NULL,
    maker_token     TEXT NOT NULL,
    side            TEXT NOT NULL,             -- bid | ask
    price_ticks     BIGINT NOT NULL,
    salt            BIGINT NOT NULL,
    expiry_ms       BIGINT NOT NULL,
    taker_amount    BIGINT NOT NULL,           -- fill cap, taker-token units
    maker_amount    BIGINT NOT NULL,
    filled_taker    BIGINT NOT NULL DEFAULT 0, -- cumulative, chain-confirmed
    status          TEXT NOT NULL DEFAULT 'OPEN',
    order_json      JSONB NOT NULL,            -- full SignedOrder wire form
    order_bytes     BYTEA NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX exchange_orders_by_market_status ON exchange_orders (registry_id, status);
CREATE INDEX exchange_orders_by_maker ON exchange_orders (maker);
CREATE INDEX exchange_orders_by_manager ON exchange_orders (manager_id);

-- Chain-confirmed fills (source of truth: FillEvents), idempotent by event id.
CREATE TABLE exchange_fills (
    tx_digest       TEXT NOT NULL,
    event_seq       BIGINT NOT NULL,
    digest          TEXT NOT NULL,
    registry_id     TEXT NOT NULL,
    maker           TEXT NOT NULL,
    taker           TEXT NOT NULL,
    base_amount     BIGINT NOT NULL,
    quote_amount    BIGINT NOT NULL,
    maker_fee       BIGINT NOT NULL,
    taker_fee       BIGINT NOT NULL,
    maker_sold_base BOOLEAN NOT NULL,
    filled_total    BIGINT NOT NULL,           -- cumulative after this fill
    timestamp_ms    BIGINT NOT NULL,
    PRIMARY KEY (tx_digest, event_seq)
);
CREATE INDEX exchange_fills_by_digest ON exchange_fills (digest);
CREATE INDEX exchange_fills_by_market_time ON exchange_fills (registry_id, timestamp_ms DESC);
CREATE INDEX exchange_fills_by_maker ON exchange_fills (maker);
CREATE INDEX exchange_fills_by_taker ON exchange_fills (taker);

-- Escrow balances mirrored from Deposit/Withdraw/Fill events (spec §5.7).
CREATE TABLE exchange_balances (
    manager_id      TEXT NOT NULL,
    token           TEXT NOT NULL,
    amount          BIGINT NOT NULL,
    PRIMARY KEY (manager_id, token)
);

-- Delegated signer sets mirrored from SignerAdded/SignerRemoved events.
CREATE TABLE exchange_approved_signers (
    manager_id      TEXT NOT NULL,
    signer          TEXT NOT NULL,
    PRIMARY KEY (manager_id, signer)
);

-- GraphQL event-stream cursors, one per module stream (persisted so a
-- restart resumes exactly, spec §5.7). Opaque server strings.
CREATE TABLE exchange_cursors (
    name            TEXT PRIMARY KEY,
    cursor          TEXT NOT NULL
);

-- Salt watermarks mirrored from SaltWatermarkEvent.
CREATE TABLE exchange_salt_watermarks (
    registry_id     TEXT NOT NULL,
    maker           TEXT NOT NULL,
    min_valid_salt  BIGINT NOT NULL,
    PRIMARY KEY (registry_id, maker)
);
