-- token-info catalog schema.
--
-- One table: the supported-token catalog. Keyed by the fully-qualified Move
-- coin type. `source` distinguishes auto-seeded rows (from deployments.json
-- testTokens on dev/staging) from rows added operationally via the internal
-- API, so a re-seed never clobbers a manual edit.

CREATE TABLE supported_tokens (
    coin_type     TEXT         PRIMARY KEY,
    ticker        TEXT         NOT NULL,
    name          TEXT         NOT NULL,
    logo_uri      TEXT,
    decimals      SMALLINT     NOT NULL,
    pyth_feed_id  TEXT,
    enabled       BOOLEAN      NOT NULL DEFAULT true,
    -- 'seed' (auto-seeded from deployments.json testTokens) | 'manual' (internal API).
    source        TEXT         NOT NULL DEFAULT 'manual',
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX supported_tokens_ticker_idx  ON supported_tokens (ticker);
CREATE INDEX supported_tokens_enabled_idx ON supported_tokens (enabled);
