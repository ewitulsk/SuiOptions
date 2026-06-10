DROP INDEX buckets_org_id_idx;
ALTER TABLE buckets DROP COLUMN org_id;
DROP TABLE orgs;
