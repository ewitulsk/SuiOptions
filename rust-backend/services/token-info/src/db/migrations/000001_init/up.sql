-- token-info catalog schema.
--
-- One table: the durable, operator-managed supported-token catalog, keyed by
-- the fully-qualified Move coin type. On dev/staging the `/tokens` endpoint
-- additionally overlays the test tokens derived from deployments.json at read
-- time (see `overlay.rs`) — those are NOT persisted here.

CREATE TABLE supported_tokens (
    coin_type     TEXT         PRIMARY KEY,
    ticker        TEXT         NOT NULL,
    name          TEXT         NOT NULL,
    logo_uri      TEXT,
    decimals      SMALLINT     NOT NULL,
    pyth_feed_id  TEXT,
    enabled       BOOLEAN      NOT NULL DEFAULT true,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX supported_tokens_ticker_idx  ON supported_tokens (ticker);
CREATE INDEX supported_tokens_enabled_idx ON supported_tokens (enabled);
