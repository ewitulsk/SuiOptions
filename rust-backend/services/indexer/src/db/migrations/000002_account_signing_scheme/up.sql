-- SO: persist the account signing scheme so the JIT GraphQL `account(id)`
-- query can return it. Previously only `signing_pubkey` was materialized
-- (the fanout consumers learned the scheme from the event payloads); JIT
-- consumers read state directly and need the scheme for signature checks.
--
-- Stored as the on-chain u8 tag (0=Ed25519, 1=Secp256k1, 2=Secp256r1).
-- Nullable: pre-existing rows are backfilled below from the event log, but a
-- row whose scheme can't be determined stays NULL (treated as "unknown
-- signer" downstream, same as before this column existed).
ALTER TABLE accounts ADD COLUMN signing_scheme SMALLINT;

-- Backfill from the most recent AccountCreated / SigningKeyRotated event per
-- account. `payload` holds the full tagged ChainEvent; the scheme is a
-- human-readable string there (serde JSON encoding of SigningScheme).
WITH scheme_events AS (
    SELECT
        payload -> 'payload' ->> 'account_id' AS account_id,
        sequence,
        CASE event_type
            WHEN 'AccountCreated'    THEN payload -> 'payload' ->> 'signing_scheme'
            WHEN 'SigningKeyRotated' THEN payload -> 'payload' ->> 'new_scheme'
        END AS scheme_str
    FROM indexed_events
    WHERE event_type IN ('AccountCreated', 'SigningKeyRotated')
),
latest AS (
    SELECT DISTINCT ON (account_id) account_id, scheme_str
    FROM scheme_events
    ORDER BY account_id, sequence DESC
)
UPDATE accounts a
SET signing_scheme = CASE latest.scheme_str
    WHEN 'ed25519'   THEN 0
    WHEN 'secp256k1' THEN 1
    WHEN 'secp256r1' THEN 2
END
FROM latest
WHERE a.account_id = latest.account_id;
