-- SO-152: DeepBook trading venues for bucket call coins.
--
-- One row per bucket that has a DeepBook pool trading its call coin against
-- the settlement asset. bucket_id PK = one pool per bucket; pool_id UNIQUE =
-- a pool maps to at most one bucket. First pool wins: ingestion inserts with
-- ON CONFLICT DO NOTHING, matching the frontend's "create only when none
-- exists" gate.
--
-- Fee/size fields are u64 on-chain; BIGINT has ample headroom (values are
-- bounded by DeepBook constants, nowhere near 2^63).
CREATE TABLE bucket_deepbook_pools (
    bucket_id            TEXT   PRIMARY KEY REFERENCES buckets (bucket_id),
    pool_id              TEXT   NOT NULL UNIQUE,
    base_asset_type      TEXT   NOT NULL,
    quote_asset_type     TEXT   NOT NULL,
    tick_size            BIGINT NOT NULL,
    lot_size             BIGINT NOT NULL,
    min_size             BIGINT NOT NULL,
    taker_fee            BIGINT NOT NULL,
    maker_fee            BIGINT NOT NULL,
    created_checkpoint   BIGINT NOT NULL,
    created_timestamp_ms BIGINT NOT NULL,
    updated_at_seq       BIGINT NOT NULL
);
