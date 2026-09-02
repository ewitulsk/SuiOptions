-- Data room DuckDB catalog (spec §9) — the single query entry point.
--
--   duckdb -init catalog/catalog.sql
--
-- Point DATA_ROOM at the lake root first:
--   s3:  SET VARIABLE root = 's3://<bucket>';   (credentials from the
--        standard AWS chain: env vars / instance profile / sso)
--   local sync: SET VARIABLE root = '/path/to/lake';
--
-- Every view prunes on hive partition columns (exchange, symbol, date).

SET VARIABLE root = getenv('DATA_ROOM');

CREATE OR REPLACE VIEW trades AS
SELECT *
FROM read_parquet(getvariable('root') || '/silver/v1/trades/**/*.parquet', hive_partitioning = true);

CREATE OR REPLACE VIEW book_top AS
SELECT *
FROM read_parquet(getvariable('root') || '/silver/v1/book_top/**/*.parquet', hive_partitioning = true);

-- L2 depth: one row per price level per update; `size` is absolute
-- (0 = level removed), `is_snapshot` marks full-book images. Reconstruct
-- with the latest snapshot at or before T plus the diffs after it.
CREATE OR REPLACE VIEW book_l2 AS
SELECT *
FROM read_parquet(getvariable('root') || '/silver/v1/book_l2/**/*.parquet', hive_partitioning = true);

-- Router execution curve: one row per (poll, direction, rung); partition
-- key is `pair`, not `symbol`.
CREATE OR REPLACE VIEW quote_ladder AS
SELECT *
FROM read_parquet(getvariable('root') || '/silver/v1/quote_ladder/**/*.parquet', hive_partitioning = true);

CREATE OR REPLACE VIEW instruments AS
SELECT *
FROM read_parquet(getvariable('root') || '/silver/v1/instruments/**/*.parquet', hive_partitioning = true);

CREATE OR REPLACE VIEW bars AS
SELECT *
FROM read_parquet(getvariable('root') || '/gold/v1/bars/**/*.parquet', hive_partitioning = true);

CREATE OR REPLACE VIEW rv AS
SELECT *
FROM read_parquet(getvariable('root') || '/gold/v1/rv/**/*.parquet', hive_partitioning = true);

CREATE OR REPLACE VIEW gaps AS
SELECT *
FROM read_parquet(getvariable('root') || '/gold/v1/gaps/**/*.parquet', hive_partitioning = true);

-- Convenience: latest instrument snapshot only.
CREATE OR REPLACE VIEW instruments_latest AS
SELECT * FROM instruments
WHERE snapshot_date = (SELECT max(snapshot_date) FROM instruments);
