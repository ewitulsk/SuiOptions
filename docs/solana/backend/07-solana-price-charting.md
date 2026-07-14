# solana-price-charting (`services/solana-price-charting`)

Port of `price-charting` **without any order-book ingestion** (per direction:
no Solana order-book integration is being nailed down yet). Ships everything
else: Timescale storage, candle aggregation, REST/WS serving, and the vault
APY sampler. Main-workspace member. Port 9011.

## What ships

- `bars.rs` (interval enum, carry-forward gap fill) — reused as-is.
- `router.rs` — `/health`, `/pools`, `/bars`, `/price-at`,
  `/vault-apy/:vault_id`, `/ws`, `/metrics`. Identical wire shapes so the
  frontend chart layer is drop-in when data appears.
- `db/` — same migrations/tables: `pool_trades`, `pool_mids`, `watch_cursor`,
  `vault_predicted_apy`, `vault_realized_apy`. The trade/mid tables stay
  empty until an ingestion source exists; the `tx_digest` column is named
  `signature` in the new migrations (fresh DB, no compat burden).
- **apy_sampler** — the genuinely live part. Ported with source swaps:
  - vaults/rounds/realized series ← `solana-indexer-graphql`.
  - Tier-1 premium evidence ← open **call auctions** for the vault's current
    bucket (indexer `auctions(status: open, mode: covered_call)` + best bids)
    replacing Sui RFQ premiums.
  - spot + realized vol ← `oracle-client` against **solana-oracle-service**.
  - Pure math (`apy/compute.rs`) unchanged.

## What is stubbed (and how, honestly)

- No `watcher.rs`, no `mid_sampler.rs` — the modules are **not ported**. No
  disabled flags, no dead config: the boot path simply spawns
  `apy_sampler` + the server. `/pools` returns `[]`, `/bars` returns empty
  arrays, `/ws` accepts subscriptions and sends nothing (clients already
  handle quiet streams). This is the honest "build up to needing an
  orderbook" boundary: when a Solana venue integration lands, it adds an
  ingestion task that writes `pool_trades`/`pool_mids` and broadcasts —
  everything downstream already works.
- Discovery config keys (`discovery_interval_secs`, `poll_interval_ms`,
  `mid_sample_interval_secs`) are omitted, `ttl_hours` kept (eviction still
  guards future ingestion).

## Config / secrets

- `bind_addr 0.0.0.0:9011`, `db_pool_size`, `ttl_hours`, `apy_tick_secs`,
  `[model]`, `[pyth]` guard keys — as Sui.
- URLs: `indexer_graphql_url` → solana-indexer, `oracle_url` →
  solana-oracle-service, `token_info_url` → solana-token-info (catalog for
  decimals). No api-service dependency (that was DeepBook discovery only). No
  RPC secret (no chain reads).
- `database_url = "${SOLANA_CHART_DATABASE_URL}"`; secret
  `options/<env>/solana-price-charting` `{"database_url": …}` — a **separate
  database on the Tiger Data instance** (`solana_tsdb`). Operator note: create
  the DB + timescale extension on the existing instance; prod's Tiger
  credential situation (prod currently borrows staging's instance) applies
  here too and is called out in the migration doc.
- Nginx route: `/{env}/solana-charts/…` with WS upgrade (mirror of `/charts`).

## Verification

- `bars.rs`/compute tests carry over. apy_sampler unit-tested against fixture
  GraphQL/oracle responses. Router integration test: empty-DB responses have
  the documented shapes.
