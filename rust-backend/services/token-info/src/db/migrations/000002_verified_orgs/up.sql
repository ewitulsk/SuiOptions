-- Verified-orgs allowlist (org-aware protocol, "verified-only surfaces").
--
-- Orgs are created permissionlessly on-chain and the indexer records them
-- all; THIS table is the platform-controlled allowlist that decides which
-- orgs' buckets api-service /buckets, quoting-service RFQs, and the frontend
-- serve. Mutations ride the same admin-JWT (public) / network-isolated
-- (internal) routes as the token catalog. "Verified" = row exists AND
-- enabled; de-verification is a PUT enabled=false so history is preserved.

CREATE TABLE verified_orgs (
    org_id     TEXT         PRIMARY KEY,
    name       TEXT         NOT NULL,
    enabled    BOOLEAN      NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX verified_orgs_enabled_idx ON verified_orgs (enabled);
