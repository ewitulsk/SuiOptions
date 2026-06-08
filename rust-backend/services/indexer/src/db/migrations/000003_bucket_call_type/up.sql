-- SO-119: per-bucket fungible option coin type. Each bucket's option is a
-- Coin<call_type> minted from a per-roll published package, so the coin type
-- is per-bucket. Mirrors `store::BucketState.call_type` / `BucketCreated`.
ALTER TABLE buckets ADD COLUMN call_type TEXT NOT NULL DEFAULT '';
