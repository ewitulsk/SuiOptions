-- SO-69: per-bucket admin invalidation flag. Freezes new writes while
-- leaving exercise/redeem flows open. Defaults to false for pre-existing
-- rows; toggled by the new BucketInvalidated / BucketRevalidated events.
ALTER TABLE buckets ADD COLUMN invalidated BOOLEAN NOT NULL DEFAULT false;
