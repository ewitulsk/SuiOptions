DROP INDEX IF EXISTS positions_object_id_idx;
ALTER TABLE positions DROP COLUMN IF EXISTS object_id;
