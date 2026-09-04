-- NFT marketplace indexer schema (P0).
-- Cursor-gated apply: every table carries first_version so reruns and
-- backfills are idempotent by construction.

-- +goose Up
CREATE TABLE IF NOT EXISTS pipeline_progress (
    name         TEXT PRIMARY KEY,
    last_version BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS activities (
    version        BIGINT NOT NULL,
    event_index   BIGINT NOT NULL,
    timestamp_us  BIGINT NOT NULL,
    marketplace   TEXT NOT NULL,
    kind          TEXT NOT NULL,
    raw_event     TEXT NOT NULL,
    listing_id    TEXT NOT NULL DEFAULT '',
    token_data_id TEXT NOT NULL DEFAULT '',
    creator       TEXT NOT NULL DEFAULT '',
    collection    TEXT NOT NULL DEFAULT '',
    token_name    TEXT NOT NULL DEFAULT '',
    property_ver  BIGINT,
    price         BIGINT,
    quote_token   TEXT NOT NULL DEFAULT '',
    buyer         TEXT NOT NULL DEFAULT '',
    seller        TEXT NOT NULL DEFAULT '',
    commission    BIGINT,
    royalty       BIGINT,
    remaining     BIGINT,
    PRIMARY KEY (version, event_index, marketplace)
);
CREATE INDEX IF NOT EXISTS activities_listing ON activities (marketplace, listing_id);
CREATE INDEX IF NOT EXISTS activities_token ON activities (token_data_id);
CREATE INDEX IF NOT EXISTS activities_time ON activities (timestamp_us DESC);

-- Current open listings/offers. Closed by fills/cancels.
CREATE TABLE IF NOT EXISTS live_listings (
    marketplace   TEXT NOT NULL,
    listing_id    TEXT NOT NULL,
    token_data_id TEXT NOT NULL DEFAULT '',
    creator       TEXT NOT NULL DEFAULT '',
    collection    TEXT NOT NULL DEFAULT '',
    token_name    TEXT NOT NULL DEFAULT '',
    property_ver  BIGINT,
    price         BIGINT NOT NULL,
    quote_token   TEXT NOT NULL DEFAULT '',
    seller        TEXT NOT NULL DEFAULT '',
    open_version  BIGINT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (marketplace, listing_id)
);
CREATE INDEX IF NOT EXISTS live_listings_collection ON live_listings (collection);
CREATE INDEX IF NOT EXISTS live_listings_seller ON live_listings (seller);

-- Allowlisted quote tokens (mirrors on-chain allowlist for pricing).
CREATE TABLE IF NOT EXISTS quote_tokens (
    address   TEXT PRIMARY KEY,
    symbol    TEXT NOT NULL DEFAULT '',
    decimals  INT NOT NULL DEFAULT 8,
    min_fee   BIGINT NOT NULL DEFAULT 0,
    enabled   BOOLEAN NOT NULL DEFAULT TRUE
);
INSERT INTO quote_tokens (address, symbol, decimals, min_fee, enabled)
VALUES ('0xa', 'APT', 8, 0, TRUE)
ON CONFLICT (address) DO NOTHING;
