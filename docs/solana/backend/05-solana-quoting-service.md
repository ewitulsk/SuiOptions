# solana-quoting-service (`services/solana-quoting-service`)

Clone of `quoting-service`: WS RFQ broker between retail frontends and MMs for
the **signed-quote `execute_write` flow** (which exists on Solana core
unchanged). Main-workspace member. Port 9002 (WS + plain HTTP `/health`,
`/metrics`, `/rfq-stream`).

## What stays identical

- WS-first server with HTTP side door; Hello-shape dispatch (MM vs retail).
- RFQ orchestration: broadcast → collect within `rfq_window_ms` →
  validate/reserve → sort best-first → respond. Bulk-view indicative quotes
  with the SWR cache. Per-session/global inflight semaphores.
- State: reservation table keyed `(account, nonce)` + 250 ms evictor,
  reputation counters, `available = on-chain balance − reservations`,
  `reconcile_executed` scan of `WriteExecuted` events to release reservations.
- Config: `bind_addr`, `indexer_graphql_url` (→ solana-indexer),
  `token_info_url` (→ solana-token-info), `rfq_window_ms`,
  `bulk_view_cache_ttl_ms`, `ping_interval_secs`, inflight caps. No secrets.

## What changes

- **protocol_id = the options_core Config PDA** (base58), fetched from
  solana-token-info at boot (`config_pda()`), embedded in every `Quote` and
  checked on validation.
- **Quote canonical bytes = Borsh** (`protocol-types` gets a
  `solana` module or the service defines the struct locally — decision:
  define `SolanaQuote` in `crates/solana-indexer-graphql`'s sibling…
  no: define it in a small `quote.rs` inside the service AND in solana-tx for
  the mm-bot; lock both against **golden vectors generated from
  `options_core::quote`** committed as fixtures, the same drift-guard idea as
  the indexer's IDL snapshots).
- **ed25519 only** (program v1): MM auth challenge and quote signatures verify
  with `ed25519-dalek` against the MM's registered `signing_pubkey` from the
  indexer's `account()` (scheme must be 0; anything else → fatal
  `auth_scheme_unknown`).
- Ids in all WS messages: base58; ints remain decimal strings (wire format
  otherwise unchanged, so the frontend WS layer ports mechanically).
- Balance model: `Account.balance(mint)` reads the indexer's per-mint balances
  (ATAs owned by the MmAccount PDA) — same shape, mint keys instead of coin
  types.

## Open design note (logged)

On Solana the retail executor must include the **Ed25519SigVerify precompile
instruction** for the MM quote in their transaction. The quoting-service
already returns the exact signature + quote fields; the frontend builds the
precompile ix from them (documented in the frontend guide). No service-side
change beyond passing the signature through — but the RFQResponse gains a
`quote_bytes_b64` field (canonical Borsh bytes) so clients don't need to
re-implement Borsh serialization to build the precompile ix.

## Verification

- Unit: quote validation paths (protocol_id/bucket/amount/expiry/signature/
  balance/nonce-duplicate), reservation eviction, sort order, borsh golden
  vectors vs program crate.
- Integration: scripted WS session (retail + fake MM) using `tokio-tungstenite`
  test harness (the Sui service's test pattern).
