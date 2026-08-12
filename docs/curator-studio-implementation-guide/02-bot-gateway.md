# 02 — P1: `bot-gateway`

The only path to the chain for hosted bots. New crate `rust-backend/services/bot-gateway`, port **9019**, publicly routed through nginx (Fly bots reach it over the internet). Model it file-for-file on the house anatomy (`token-info` for the HTTP shape, `staging-mm-bot` for the trading logic — reuse its `client.rs`/`signing.rs`/budget logic nearly verbatim).

```
services/bot-gateway/
  Cargo.toml                    [lib] + [[bin]]; axum 0.7 (+"ws"), diesel, tower-http
  config/config.{toml,staging.toml,prod.toml}
  src/{main,lib,config,router,state}.rs
  src/handlers/{mod,orders,markets,vaults,heartbeat,control}.rs
  src/{salt.rs,budget.rs,risk.rs,watermark.rs,keysvc.rs}
  src/db/{mod,models,schema,repo}.rs + src/db/migrations/000001_init/
```

## 1. Bootstrap (the canonical 5 steps + trading deps)

```
observability::init("bot-gateway") → Cli::parse (--config, env flags: TOKEN_INFO_URL,
ORACLE_URL, ORDERBOOK_URL, API_URL, KEY_SERVICE_URL) → Config::load
→ establish_pool + run_migrations (diesel embedded)
→ TokenInfoClient::fetch_blocking_until_ready(30, 2s)     # bounded_curator pkg ids etc.
→ wait_for_markets(orderbook, 60, 5s)                     # registry ids = signature domain
→ oracle_client::OracleClient::subscribe() → PriceCache
→ load bot registry rows (provisioner-written) → per-vault SaltSource + watermark state
→ router::serve(bind_addr)                                # /health binds last = readiness
```

No `Secrets`/`Signer` here: **the gateway holds no keys.** All signatures (order digests and PTB sign-bytes) come from key-service over the internal network (03).

## 2. AuthN/AuthZ

- Per-bot **vault-scoped API tokens**, issued by the provisioner, stored hashed (blake2/argon2) in `bot_tokens`. `Authorization: Bearer cs_bot_<id>_<secret>`; middleware resolves token → `BotCtx { bot_id, vault_id, spec_snapshot }` and injects it. Revocation = row delete.
- Dashboard/control calls come through the existing admin-JWT path (`crates/auth-client::require_auth`) on separate routes.

## 3. API surface

```
GET  /health
# bot-facing (Bearer bot token; all scoped to the token's vault)
POST /v1/orders                 { market, side, priceTicks|price, lots, ttlMs? } → intent
DELETE /v1/orders/:digest       soft-cancel passthrough
POST /v1/orders/cancel-all      soft-cancel + queue watermark raise
GET  /v1/markets                proxied+cached from orderbook (adds studio metadata)
GET  /v1/markets/:m/book        proxied from orderbook GET /v1/markets/:m/book
GET  /v1/vault/state            balances, positions, pending-request obligations, limiter state
GET  /v1/prices                 snapshot from PriceCache
POST /v1/heartbeat              { seq, state, openOrders } → { control: "run"|"pause"|"kill" }
WS   /v1/ws                     market data relay + control-plane push (pause/kill)
# ops/admin (JWT)
POST /admin/bots/:id/pause | /resume | /kill
GET  /admin/bots/:id/status
```

Wire structs `#[serde(rename_all = "camelCase")]`; typed rejections mirror the orderbook's `{"error":{"code","detail"}}` 422 shape so the Python SDK surfaces one error model.

## 4. The order path (the heart of the service)

For an intent on a hybrid-exchange market:

1. **Risk tier** (`risk.rs`): validate against the vault's deployed spec snapshot — market whitelisted, order notional ≤ per-order cap, projected open notional ≤ spec cap, drawdown stop not tripped (from `budget.rs` P&L tracking). Reject with typed codes; the on-chain limiter remains the floor.
2. **Budget** (`budget.rs`, port of `staging-mm-bot::quote_budget` direct mode): vault live free balance − `buffer_bps` − outstanding withdrawal obligations (api-service `GET /trading-vaults/:id/pending-requests`).
3. **Price sanity**: compute mid from `PriceCache` (staleness gates exactly like `staging-mm-bot::Staleness { max_price_age 5s, max_publish_lag 10s }`); reject intents when the price plane is stale (the bot's control channel gets `pause` — park, don't crash).
4. **Order construction**: canonical token types (`canonicalize_move_type`), amounts on the `(tick_size, lot_size)` grid, `expiry_ms` within the orderbook's `[30s, 24h]` window, `maker` = **vault address**, `maker_manager_id` = the guarded custody BM, `salt = SaltSource::next()` — one serialized allocator **per vault** (`salt.rs`; time-seeded AtomicU64 like `staging-mm-bot`, persisted high-water mark in `bot_salts` so restarts never regress).
5. **Sign**: `exchange_signing::order_digest(&order, &registry_id)` → POST key-service `/internal/sign/order { vaultId, digest }` → 64-byte ed25519 sig.
6. **Submit**: `POST {orderbook}/v1/orders` with the `SignedOrder`. Map `IntakeReject` codes: `INSUFFICIENT_ESCROW` → re-budget; `SALT_*` → allocator bug (alert); others → surface to bot.
7. **Record** the intent row (`bot_intents`) — the audit trail joining bot intent → order digest → fills.

Fills reach bots via the gateway's WS relay of orderbook `orders.{vaultAddr}` frames (`ack`/`fill{final}`/`cancelled`/`pruned`).

**Watermarks** (`watermark.rs`, port of `staging-mm-bot`): soft-cancel is not an on-chain void. Per vault, track `pending`/`raised`; raise reactively on kill/pause and hourly by sweep via `sui_tx::tx::exchange::cancel_up_to_batch` with `manager_id = Some(custody_bm)` (routes through `settlement::cancel_up_to_for_manager` since the maker is the vault address). PTB signing goes through key-service `/internal/sign/tx`. Never submit at or below `raised` (`EWatermarkRegression` fails the whole batch). Failures: `error!(alert_id = "tx-failed-bot-gateway-watermark", …)`.

## 5. Guarded PTB construction

New builder module `crates/sui-tx/src/tx/bounded_curator.rs`, following the house shape (`(&ChainClient, ids…, gas_budget)`, `build_*` variants for composition):

- `build_attest(provider, feed_ids…) `— assembles `oracle_pyth::attest` or `oracle_switchboard::attest` depending on the active provider (resolved at call time from token-info + oracle-service `/oracle/descriptor`; never cached across calls).
- `guarded_place_limit_order`, `guarded_taker_swap_*` — attest → `bounded_curator::guarded_*` with `limiter` shared object arg (`shared_object_arg(client, limiter_id, true)`), clock `0x6`.
- `wrap`, `unwrap_tx_bytes`, `rotate_curator_tx_bytes` — the latter two produce **sign-ready tx bytes for the user's wallet** (dashboard deep-links, 07 §3).
- Gas: curator wallets are funded with SUI by the provisioner; `gas_tx_data` already handles address-balance gas (SO-366), so no coin-object management is needed on the hot path.

## 6. Orderbook Submitter change (small, separate PR)

`services/orderbook/src/settlement.rs::Submitter` targets `exchange_adapter::match_*` for direct-vault legs today. For studio vaults it must:
- detect guarded custodies (new `exchange_vault_managers.guarded` flag, populated by the chain-sync `VaultCustody` handler recognizing `guarded_exchange_adapter::CustodyCreated`);
- target `guarded_exchange_adapter::match_*` and prefix the PTB with the oracle attest call (limiter + attestation params, 01 §5);
- map the new abort codes: band/notional violations → prune that maker's orders in that market (the quotes are mispriced or over-budget — same reaction class as `VaultEscrowInsufficient`); stale-attestation aborts → `Stale` (restore + re-match).

Also extend `GET /v1/markets`' `directEscrow` block with the guarded adapter's `adapterPackageId` variant so open-orderbook takers can build guarded fills.

## 7. DB schema (diesel, `bot_gateway_<env>`)

```
bot_tokens      (id, vault_id, token_hash, created_at, revoked_at)
bots            (id, vault_id, spec_snapshot JSONB, state TEXT, last_heartbeat_at, control TEXT)
bot_salts       (vault_id PK, high_water BIGINT)
bot_watermarks  (vault_id, registry_id, pending BIGINT, raised BIGINT)
bot_intents     (id, bot_id, vault_id, registry_id, side, price_ticks, lots, salt,
                 digest, status, reject_code, created_at)
```

## 8. Config sketch

```toml
# services/bot-gateway/config/config.staging.toml
bind_addr = "0.0.0.0:9019"
network = "testnet"
database_url = "postgresql://bot_gateway_staging:${DB_PASSWORD}@${DB_HOST}:5432/bot_gateway_staging"
allowed_origins = ["*"]            # Fly bots + dashboard
[risk]
buffer_bps = 1000
max_order_ttl_secs = 90
[staleness]
max_price_age_ms = 5000
max_publish_lag_ms = 10000
[watermark]
sweep_interval_secs = 3600
```

Peer URLs via compose env (`TOKEN_INFO_URL=http://token-info:9005`, `ORACLE_URL=http://oracle-service:9013`, `ORDERBOOK_URL=http://orderbook:9014`, `API_URL=http://api-service:9003`, `KEY_SERVICE_URL=http://key-service:9023`).

## 9. Alert ids

`tx-failed-bot-gateway-watermark`, `tx-failed-bot-gateway-guarded-order`, plus non-tx `bot-heartbeat-missed` (fired by the heartbeat monitor when a RUNNING bot goes quiet past 3× its interval) and `bot-restart-loop`. Append to `.claude/tx-alerting.md`.
