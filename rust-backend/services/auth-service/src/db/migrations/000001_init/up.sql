-- auth-service identity store.
--
-- Deliberately holds NO personally identifying information. `identities.
-- identifier` is either a username (an opaque handle the user picks) or a Sui
-- address — never an email, legal name, or anything sourced from KYC. Any
-- future method must keep that property.

CREATE TABLE users (
    id          UUID PRIMARY KEY,
    -- 'admin' | 'business' | 'individual'. Authorization is role + scope_id;
    -- there is nothing finer-grained.
    role        TEXT        NOT NULL,
    -- Opaque to this service. dakota-service reads it as a Dakota customer
    -- KSUID; auth-service never interprets it. NULL for admins, who are
    -- unscoped.
    scope_id    TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at TIMESTAMPTZ
);

CREATE INDEX users_scope_id_idx ON users (scope_id) WHERE scope_id IS NOT NULL;

-- One row per login method. A user may hold several, which is how "set a
-- password on my wallet account" and "add a wallet to my password account"
-- both work: they insert a second row against the same user_id.
CREATE TABLE identities (
    id          UUID PRIMARY KEY,
    user_id     UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 'password' | 'sui_wallet'. The extension point: a future 'passkey' or
    -- 'oauth:google' is a new value here and nothing else.
    kind        TEXT        NOT NULL,
    -- Username for 'password', normalized 0x-address for 'sui_wallet'.
    identifier  TEXT        NOT NULL,
    -- Argon2id PHC string for 'password'; NULL for signature-proved methods.
    secret_hash TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,

    -- One account per (method, identifier): a username or wallet cannot be
    -- claimed twice.
    UNIQUE (kind, identifier)
);

CREATE INDEX identities_user_id_idx ON identities (user_id);

-- Signup grants. dakota-service mints one when an admin creates a partner
-- business, or when a business invites one of its own customers; the invitee
-- redeems it at POST /register. Single-use and time-boxed.
CREATE TABLE invites (
    id          UUID PRIMARY KEY,
    role        TEXT        NOT NULL,
    scope_id    TEXT,
    -- NULL when minted by an internal service rather than a logged-in user.
    created_by  UUID        REFERENCES users (id) ON DELETE SET NULL,
    label       TEXT,
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    consumed_by UUID        REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX invites_open_idx ON invites (expires_at) WHERE consumed_at IS NULL;
