-- Hybrid exchange orderbook schema (spec §5.1 `store`).
--
-- Amounts are stored as BIGINT: Sui amounts are u64 but every practical coin
-- amount fits i64; intake rejects orders whose amounts exceed i64::MAX.

CREATE TABLE markets (
    registry_id     TEXT PRIMARY KEY,
    symbol          TEXT NOT NULL,
    base            TEXT NOT NULL,
    quote           TEXT NOT NULL,
    tick_size       BIGINT NOT NULL,
    min_size        BIGINT NOT NULL,
    lot_size        BIGINT NOT NULL,
    current_fee_bps BIGINT NOT NULL
);

-- Signed orders. `status`: OPEN | FILLED | CANCELLED | PRUNED | EXPIRED.
-- BCS bytes + digest are computed once at intake and cached (spec §8).
CREATE TABLE orders (
    digest          TEXT PRIMARY KEY,
    registry_id     TEXT NOT NULL REFERENCES markets (registry_id),
    maker           TEXT NOT NULL,
    manager_id      TEXT NOT NULL,
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
CREATE INDEX orders_by_market_status ON orders (registry_id, status);
CREATE INDEX orders_by_maker ON orders (maker);
CREATE INDEX orders_by_manager ON orders (manager_id);

-- Chain-confirmed fills (source of truth: FillEvents), idempotent by event id.
CREATE TABLE fills (
    tx_digest       TEXT NOT NULL,
    event_seq       TEXT NOT NULL,
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
CREATE INDEX fills_by_digest ON fills (digest);
CREATE INDEX fills_by_market_time ON fills (registry_id, timestamp_ms DESC);
CREATE INDEX fills_by_maker ON fills (maker);
CREATE INDEX fills_by_taker ON fills (taker);

-- Escrow balances mirrored from Deposit/Withdraw/Fill events (spec §5.7).
CREATE TABLE balances (
    manager_id      TEXT NOT NULL,
    token           TEXT NOT NULL,
    amount          BIGINT NOT NULL,
    PRIMARY KEY (manager_id, token)
);

-- Delegated signer sets mirrored from SignerAdded/SignerRemoved events.
CREATE TABLE approved_signers (
    manager_id      TEXT NOT NULL,
    signer          TEXT NOT NULL,
    PRIMARY KEY (manager_id, signer)
);

-- Event-stream cursor (persisted so restart resumes exactly, spec §5.7).
CREATE TABLE cursors (
    name            TEXT PRIMARY KEY,
    tx_digest       TEXT NOT NULL,
    event_seq       TEXT NOT NULL
);

-- Salt watermarks mirrored from SaltWatermarkEvent.
CREATE TABLE salt_watermarks (
    registry_id     TEXT NOT NULL,
    maker           TEXT NOT NULL,
    min_valid_salt  BIGINT NOT NULL,
    PRIMARY KEY (registry_id, maker)
);
