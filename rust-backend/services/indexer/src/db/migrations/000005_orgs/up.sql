-- Permissionless orgs: every bucket now belongs to an org (the OrgCap that
-- created it). The indexer records ALL orgs (complete chain record); the
-- verified-orgs allowlist that gates user-facing surfaces lives in
-- token-info, not here.
CREATE TABLE orgs (
    org_id         TEXT   PRIMARY KEY,
    name           TEXT   NOT NULL,
    fee_bps        BIGINT NOT NULL,
    creator        TEXT   NOT NULL,
    updated_at_seq BIGINT NOT NULL
);

-- Fresh deploys start with an empty buckets table; the DEFAULT '' keeps the
-- migration runnable on a pre-org staging DB (those rows are from the old
-- package and are never served once the indexer points at the new one).
ALTER TABLE buckets ADD COLUMN org_id TEXT NOT NULL DEFAULT '';
CREATE INDEX buckets_org_id_idx ON buckets (org_id);
