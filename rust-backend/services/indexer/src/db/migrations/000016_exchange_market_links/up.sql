-- SO-416: in-house exchange option markets for buckets.
--
-- One row per bucket that has an exchange market trading its option coin
-- against the settlement asset (permissionless listings, SO-415). bucket_id
-- PK = one market per bucket; registry_id UNIQUE = a market maps to at most
-- one bucket. First listing wins: ingestion inserts with ON CONFLICT DO
-- NOTHING, matching the listing package's one-market-per-series dedup.
CREATE TABLE exchange_market_links (
    bucket_id      TEXT PRIMARY KEY REFERENCES buckets (bucket_id),
    registry_id    TEXT NOT NULL UNIQUE,
    is_put         BOOLEAN NOT NULL,
    updated_at_seq BIGINT NOT NULL
);
