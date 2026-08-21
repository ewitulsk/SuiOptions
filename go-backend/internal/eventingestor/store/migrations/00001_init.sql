-- event-ingestor schema, v1.
--
-- DB-driven event→points rules: no config files. Admin adds a package
-- (introspection cached in modules_json for the UI), configures per-event
-- rules; the poller walks Sui GraphQL module streams with persisted cursors.

-- +goose Up

CREATE TABLE tracked_packages (
    id              BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Canonical (padded) address; lookups normalize before compare.
    package_address TEXT        NOT NULL UNIQUE,
    label           TEXT        NOT NULL DEFAULT '',
    -- Cached chain introspection: {package, modules:[{name, structs:[…]}]}.
    -- Refresh in v1 = delete + re-add.
    modules_json    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    introspected_at TIMESTAMPTZ,
    created_by      TEXT        NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE event_rules (
    id              BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    package_address TEXT        NOT NULL REFERENCES tracked_packages(package_address) ON DELETE CASCADE,
    module_name     TEXT        NOT NULL,
    -- Canonical '0x<64>::module::Struct' — matched against event contents.
    event_type      TEXT        NOT NULL,
    label           TEXT        NOT NULL DEFAULT '',
    points          BIGINT      NOT NULL,
    recipient_mode  TEXT        NOT NULL CHECK (recipient_mode IN ('sender', 'field')),
    recipient_field TEXT,
    start_mode      TEXT        NOT NULL CHECK (start_mode IN ('tip', 'timestamp')),
    start_at        TIMESTAMPTZ,
    backfill_state  TEXT        NOT NULL DEFAULT 'none'
                    CHECK (backfill_state IN ('none', 'pending', 'running', 'done', 'exhausted')),
    backfill_cursor TEXT,
    enabled         BOOLEAN     NOT NULL DEFAULT true,
    created_by      TEXT        NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (package_address, module_name, event_type)
);

CREATE INDEX event_rules_enabled_idx ON event_rules(enabled);

-- One cursor per module stream. The value embeds its package
-- ("{pkg}|{opaque_cursor}") so a republished package self-heals to a tip
-- re-init instead of resuming an orphaned stream position (the
-- exchange_watcher.rs pattern).
CREATE TABLE module_cursors (
    package_address TEXT        NOT NULL,
    module_name     TEXT        NOT NULL,
    cursor          TEXT,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (package_address, module_name)
);

-- Delivery audit + re-POST skip. The leaderboard's global UNIQUE
-- idempotency_key remains the true dedupe authority; this table just makes
-- replays cheap and gives /status its delivered counts.
CREATE TABLE deliveries (
    id              BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    rule_id         BIGINT      NOT NULL REFERENCES event_rules(id) ON DELETE CASCADE,
    idempotency_key TEXT        NOT NULL UNIQUE,
    recipient       TEXT        NOT NULL,
    points          BIGINT      NOT NULL,
    event_time      TIMESTAMPTZ NOT NULL,
    delivered_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX deliveries_rule_time_idx ON deliveries(rule_id, delivered_at DESC);
