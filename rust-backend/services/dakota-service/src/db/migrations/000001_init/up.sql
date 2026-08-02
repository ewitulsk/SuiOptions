-- dakota-service read model.
--
-- POLICY: this schema stores NO personally identifying information. Every
-- Dakota response we touch is full of it — `GET /customers` returns `email`
-- and `name`, `POST /accounts` returns `bank_account.account_holder_name` and
-- `account_number`, `GET /events` returns `sender_details.sender_account_name`
-- and `sender_account_number`. None of that lands here.
--
-- What we keep is the skeleton needed to aggregate and authorize: Dakota
-- KSUIDs, enums, amounts, assets and timestamps. Anything a human would
-- recognize as a person is fetched from Dakota per-request and relayed
-- straight to the browser. Adding a `name` column here would quietly break
-- that promise, so don't.

-- Admin-curated catalog of what we support. Dakota has no assets endpoint —
-- `/capabilities/networks` returns bare network ids and nothing about assets —
-- so this table IS the source of truth for every dropdown in the dashboard.
CREATE TABLE assets (
    id              SERIAL PRIMARY KEY,
    symbol          TEXT        NOT NULL,
    network_id      TEXT        NOT NULL,
    onramp_enabled  BOOLEAN     NOT NULL DEFAULT false,
    offramp_enabled BOOLEAN     NOT NULL DEFAULT false,
    swap_enabled    BOOLEAN     NOT NULL DEFAULT false,
    sort_order      INTEGER     NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (symbol, network_id)
);

-- Expected fee schedule. `GET /self-serve/credits/pricing` 403s for our client
-- tier ("Credit management is only available for self-serve customers"), so
-- `source = 'manual'` is the only row we can actually produce today. The
-- 'dakota' source exists so a future tier change needs no migration.
CREATE TABLE fee_schedule (
    id                SERIAL PRIMARY KEY,
    source            TEXT        NOT NULL DEFAULT 'manual',
    transfer_fee_bps  INTEGER,
    ach_fee_cents     INTEGER,
    wire_fee_cents    INTEGER,
    sepa_fee_cents    INTEGER,
    swift_fee_cents   INTEGER,
    kyc_fee_cents     INTEGER,
    kyb_fee_cents     INTEGER,
    effective_from    TIMESTAMPTZ NOT NULL DEFAULT now(),
    fetched_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    note              TEXT
);

-- Customer skeleton. No name, no email — see the policy note above.
CREATE TABLE customers (
    dakota_customer_id     TEXT PRIMARY KEY,
    customer_type          TEXT        NOT NULL,
    is_sub_client          BOOLEAN     NOT NULL DEFAULT false,
    -- The partner business this customer belongs to, if any. This is what
    -- makes the three-tier hierarchy queryable without asking Dakota.
    sub_client_id          TEXT,
    external_ref           TEXT,
    application_id         TEXT,
    kyb_status             TEXT,
    kyc_status             TEXT,
    application_status     TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX customers_sub_client_idx ON customers (sub_client_id) WHERE sub_client_id IS NOT NULL;
CREATE INDEX customers_is_sub_client_idx ON customers (is_sub_client) WHERE is_sub_client;

CREATE TABLE accounts (
    dakota_account_id      TEXT PRIMARY KEY,
    dakota_customer_id     TEXT        NOT NULL REFERENCES customers (dakota_customer_id) ON DELETE CASCADE,
    account_type           TEXT        NOT NULL,
    source_asset           TEXT,
    source_network_id      TEXT,
    destination_asset      TEXT,
    destination_network_id TEXT,
    rail                   TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX accounts_customer_idx ON accounts (dakota_customer_id);

-- The activity ledger behind every flow-tracking view.
--
-- Keyed on Dakota's `X-Dakota-Event-ID` so redeliveries are idempotent —
-- Dakota retries ~10 times over 48h and does NOT guarantee ordering, so this
-- table is a set of observations, not a sequence. Treat the resource's current
-- status as authoritative rather than the newest row.
--
-- Note what is absent: no raw payload column. Dakota's event bodies carry
-- sender names and bank account numbers, so we extract the handful of
-- non-identifying fields below and drop the rest on the floor.
CREATE TABLE ledger_events (
    event_id           TEXT PRIMARY KEY,
    event_type         TEXT        NOT NULL,
    resource_type      TEXT,
    resource_id        TEXT,
    dakota_customer_id TEXT,
    direction          TEXT,
    amount_minor       BIGINT,
    asset              TEXT,
    exchange_rate      TEXT,
    fee_minor          BIGINT,
    status             TEXT,
    occurred_at        TIMESTAMPTZ,
    received_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ledger_events_customer_idx ON ledger_events (dakota_customer_id, occurred_at DESC);
CREATE INDEX ledger_events_resource_idx ON ledger_events (resource_id);
CREATE INDEX ledger_events_type_idx ON ledger_events (event_type);

CREATE TABLE wallets (
    dakota_wallet_id TEXT PRIMARY KEY,
    address          TEXT,
    family           TEXT        NOT NULL,
    signer_group_id  TEXT,
    policy_id        TEXT,
    label            TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Webhooks we could not verify or parse. Stores a SHA-256 of the body, never
-- the body: a delivery that failed to parse is exactly as likely to contain
-- PII as one that succeeded.
CREATE TABLE webhook_errors (
    id          SERIAL PRIMARY KEY,
    event_id    TEXT,
    reason      TEXT        NOT NULL,
    body_sha256 TEXT        NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
