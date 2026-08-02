-- Provider-neutral feed keys (SO-335).
--
-- The catalog held exactly one feed identifier, named for its provider.
-- Switchboard identifies a feed by a 32-byte "feed hash" — same shape,
-- different value and different issuer — so it needs its own column
-- rather than overloading the Pyth one: during the migration BOTH must
-- be resolvable at once, since the two adapters run in parallel and the
-- provider is a runtime switch.
--
-- Nullable and unbacked by a default: a token with no Switchboard feed
-- is legitimate (synthetic test tokens have neither), and consumers
-- already fail loudly at startup when a configured symbol lacks the feed
-- their provider needs.
ALTER TABLE supported_tokens
    ADD COLUMN switchboard_feed_id TEXT;
