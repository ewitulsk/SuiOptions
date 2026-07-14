# solana-token-info (`services/solana-token-info`)

Clone of `services/token-info`, Solana-flavored. **Sole reader** of
`solana-deployments.json`; every other Solana service reads through
`crates/solana-token-info-client` (hard cutover, crash-if-down).

## What stays identical

- Two routers / two ports: public **9005** (nginx `/{env}/solana-token-info/…`),
  internal **9006** (no auth, + `/metrics`).
- Postgres catalog table + diesel/r2d2 + embedded migrations; `Repo`
  list/get/upsert/delete; DB `solana_token_info_<env>`.
- Read-time overlay of test tokens from the deployments file (never persisted;
  DB wins on collision), gated `network != "mainnet-beta"`; `seed_meta`
  name/logo config with the case-insensitive lookup.
- Admin-JWT gate on public mutations, delegated via `auth-client` — pointed at
  **solana-auth-service** internal `/verify` (own JWT domain; see auth guide).
- Config keys: `environment`, `network`, `deployments_path`, `database_url`,
  `db_pool_size`, `public_bind_addr`, `internal_bind_addr`, `allowed_origins`,
  `auth_service_url`, `seed_meta`.

## What changes

- **Token identity = mint address** (base58). Table PK column named `mint`
  (TEXT). `normalize_coin_type` is deleted — base58 comparison is byte-exact;
  the only validation is `bs58::decode == 32 bytes`. Collision key between DB
  and overlay is the raw mint string.
- **`GET /program-info`** replaces `/package-info`: returns the `program_info`
  block verbatim (program ids, configPda, treasuryPda, admin, network,
  testTokens). New name because the shape is new; the client crate hides it.
- DTO: `SupportedToken { mint, ticker, name, logo_uri?, decimals, pyth_feed_id?, enabled }`
  — defined in `crates/solana-token-info-client` and shared with the service
  (same pattern as today).
- Routes: `GET /tokens`, `GET /tokens/:mint`, `GET /program-info`, `GET /health`;
  `POST /tokens`, `PUT /tokens/:mint`, `DELETE /tokens/:mint` (JWT on public).

## crates/solana-token-info-client

Mirrors `token-info-client`:

- `Snapshot { program_info: ProgramInfo, tokens: Vec<SupportedToken> }` with
  accessors: `core_program()`, `venue_program()`, `vault_program()`,
  `config_pda()` (**the quote `protocol_id`**), `treasury_pda()`, `admin()`,
  `network()`, `test_tokens()`, `faucet_token(sym)`, `tokens()`,
  `token_spec(ticker)` (case-insensitive), `token_by_mint(mint)` (exact).
- `TokenInfoClient::new(url).fetch()` = GET `/program-info` + `/tokens`;
  `fetch_blocking_until_ready(30, 2s)` boot discipline.
- All ids are `String` (base58) — **no solana-sdk dep**, so the crate is
  importable from both the main workspace and the standalone services.
- `crates/solana-deployments` (loader): `SolanaDeployments::load(path)`,
  `for_env(env) -> SolanaNetworkDeployment { program_info, token_info }` —
  serde types shared with the client crate re-exports, mirroring how
  `deployments` ⇄ `token-info-client` compose today.

## Deployment

Main-workspace member. `Dockerfile.solana-token-info` copies
`solana-deployments.json` into the image bundle the same way token-info's
deploy bundle ships `deployments.json` (`/app/deployments/solana-deployments.json`
via the deploy bundle — see infra guide). Secret: auto-generated
`options/<env>/solana-token-info` db_password (terraform `random_password`
pattern, like token-info). Wipe-provision: DB prefix `solana_token_info`.

## Verification

- Unit: overlay build (DB-wins collision on mint), handler tests (list/get/
  upsert/delete, enabled filter), program-info passthrough.
- Integration (local): run against local Postgres 7654 + repo
  `solana-deployments.json`, assert `/tokens` merges overlay, `/program-info`
  round-trips.
