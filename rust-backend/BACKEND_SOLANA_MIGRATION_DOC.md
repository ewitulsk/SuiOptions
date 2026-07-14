# Backend Solana Migration

High-level overview of the Solana port of every rust-backend service. Each
Solana service runs **alongside** its Sui twin — nothing Sui-side changed
behavior. Detailed per-service design guides: `docs/solana/backend/NN-*.md`.
Frontend contracts: `services/<name>/<NAME>_FRONTEND_INTEGRATION_GUIDE.md`.

## Topology

```
programs (options_core / auction_venue / options_vault, Anchor, devnet)
  → Helius LaserStream → solana-indexer (GraphQL :9002)
      → solana-api-service :9003  → frontend REST  (/{env}/solana-api)
      → solana-quoting-service :9002 (WS RFQ broker, /{env}/solana-quoting)
      → solana-keeper (vault cranks) / solana-option-scheduler (rolls)
      → solana-price-charting :9011 (APY sampler; /{env}/solana-charts)
solana-deploy → solana-deployments.json → solana-token-info :9005/:9006
solana-oracle-service :9013 ← Pyth Hermes (SSE + Benchmarks)
solana-gas-station :9009 (fee-payer sponsorship + test-token faucet)
solana-auth-service :9007/:9008 (admin JWTs for solana-token-info writes)
solana-balance-monitor :9012 (SOL balances → low-balance alerts)
```

Public API convention: `/{environment}/{service-name}/…` with service names
`solana-token-info`, `solana-auth`, `solana-api`, `solana-quoting`,
`solana-charts`, `solana-gas-station`, `solana-indexer`.

## New shared crates

| Crate | Workspace | Role |
|---|---|---|
| `crates/solana-deployments` | main | Typed loader for `solana-deployments.json` (base58 strings, no solana-sdk). |
| `crates/solana-token-info-client` | main | HTTP client → solana-token-info (`Snapshot`, hard cutover). `config_pda()` is the quote `protocol_id`. |
| `crates/solana-indexer-graphql` | main | GraphQL client → solana-indexer (buckets/accounts/positions/auctions/vaults/events, `finalizedOnly` support, sequence-cursor scans). |
| `crates/solana-tx` | **standalone** | The `sui-tx` analog: keypair loading, Helius-aware RPC wrapper, all PDA derivations, instruction builders generated **from the real program crates** (zero drift), ed25519 quote precompile helpers (golden-tested against `options_core`), Pyth receiver `post_update_atomic`/`reclaim_rent`, Anchor error-code extraction + Benign/Retry/Fatal classification. |

`runtime-config` gained an additive `[solana]` secrets section
(`solana_keypair(network)`, `resolve_solana_rpc_url`).

Workspace isolation rule (forced by the Sui git pins in the root workspace):
anything touching solana-sdk is a **standalone cargo workspace**
(solana-indexer precedent) — solana-tx, solana-deployment-manager,
gas-station, keeper, option-scheduler, mm-bot, balance-monitor. HTTP-only
services are main-workspace members using base58 strings + borsh mirror
structs.

## Services

### tools/solana-deployment-manager (bin `solana-deploy`)
Owns initialization + the `solana-deployments.json` registry. **Program binary
deploys stay with the anchor CLI** (`solana-contracts/scripts/deploy-devnet.sh`)
— Solana program ids are deploy-stable, so the Sui tool's publish machinery
has no analog. Idempotent: re-runs converge (Config existence checked,
recorded mints probed on-chain). `--deploy-tokens` creates TUSDC/6, TBTC/8,
TSOL/9 SPL mints with **mint authority handed to `--faucet-authority`** (the
gas-station key) and seeds pyth feed ids (carry-forward preserved). LiteSVM
e2e test against the real `.so`s.

### solana-token-info (9005 pub / 9006 int, DB `solana_token_info_<env>`)
Sole reader of `solana-deployments.json`. Catalog PK = **mint** (base58,
byte-exact — the Sui `normalize_coin_type` has no analog). `GET /program-info`
replaces `/package-info`. Test-token overlay gated `network != "mainnet-beta"`.
Admin writes JWT-gated via **solana-auth-service**.

### solana-auth-service (9007 pub / 9008 int)
Clone of auth-service with Solana `signMessage` login: ed25519 over raw
message bytes, address = base58 pubkey, distinct challenge prefix so Sui/Solana
challenges can't cross-replay. **Separate JWT domain** (own secret) by design.

### solana-api-service (9003)
Stateless JIT-GraphQL REST read model. `/auctions` (+`/auctions/:id/bids`)
replaces `/rfqs` (status `open|settled|unsold`, mode
`swap|covered_call|cash_secured_put`). `tradeable = !cleaned && !invalidated
&& !expired` (no order book → no pool condition). Vault live read =
`getAccountInfo` + discriminator-checked borsh mirror (best-effort). PnL FIFO
ledger without the DeepBook/BalanceManager leg; exercise marks fall back to
strike until charting has data.

### solana-quoting-service (9002, WS)
Signed-quote RFQ broker, ported 1:1. `protocol_id` = options_core **Config
PDA**. Quote bytes = **Borsh** (golden-vectored, 160 bytes), ed25519 only.
`RFQResponse` quotes carry `quote_bytes_b64` so the frontend can build the
Ed25519SigVerify precompile ix without re-implementing Borsh.

### solana-oracle-service (9013, internal)
The Pyth gateway engine is shared: `oracle-service` got a minimal seam
refactor (feed discovery lifted into each binary; `oracle_service::run(config,
secrets, feeds)`), and solana-oracle-service is a thin binary discovering
feeds from the solana-token-info catalog. hermes-beta while on devnet.

### solana-price-charting (9011, Timescale)
Storage + candle math (`bars.rs` verbatim) + REST/WS + APY sampler — **no
order-book ingestion by design** (`watcher`/`mid_sampler` not ported; no
disabled-code stubs). `/pools`/`/bars` return empty until a Solana venue
integration lands; wire shapes identical to Sui charts so the frontend is
drop-in. APY tier-1 premium evidence comes from the vault's venue auctions
(settled `net_proceeds` + open best bids, creator-filtered to the vault PDA).
Trades key on `signature`. DB: separate `solana` database on the Tiger
instance (`SOLANA_CHART_DATABASE_URL`).

### solana-gas-station (9009)
Fee-payer co-signing sponsorship (see decision #2): validates a user-built
`VersionedTransaction` (fee payer = station key) against an
instruction-template allow-list built from the program crates' Anchor
discriminators, rejects lookup-table transactions, enforces the station key
appears **only** as fee payer, simulates and caps the station's lamport delta
(`max_sponsor_lamports_per_tx`, default 0.005 SOL — covers fees + ATA rent),
then co-signs; the wallet signs and the frontend submits. Also hosts
`POST /faucet` (non-mainnet; station key is the test mints' authority).
Template lockstep rule carries over from Sui: **every new frontend flow needs
a matching template here or sponsorship 422s.**

### solana-keeper (ops 8086)
Permissionless vault crank driver; planner/slicing/strike logic ported
verbatim (ms-native). Pyth leg: per-feed persistent `PriceUpdateV2` accounts
posted via `post_update_atomic` (Hermes → receiver), crank + posts packed in
one tx when ≤1232 bytes else split. Anchor-error classification →
Benign/Retry/Fatal with `alert_id="tx-failed-solana-keeper"`. The **LiteSVM
e2e test drives a complete round against the real programs** (deposit →
select → auction → MM bid → settle → redeem → swap → finalize), every step
chosen by the real planner.

### solana-option-scheduler (ops 8087, DB `solana_scheduler_<env>`)
Massively simpler than Sui: option/share mints are PDAs, so rolls are just N
`create_bucket` calls — no codegen/compile/publish. Deterministic salts
(hash of bucket parameters) make submits idempotent; DB partial-unique index
remains the dedup source of truth; reconciler resolves ambiguity via
`getSignatureStatuses` first, then finalized-tier indexer events. Vault salts
carry a **generation counter** (bumped per retired vault) so a paused vault's
replacement derives a fresh PDA; the adopt-on-collision path refuses paused
vaults. Strike grids enforce an integrality rule (strike × min-slice exact in
settlement units) to avoid the programs' known settlement-dust rounding issue.

### solana-mm-bot (ops 9010)
Two strategies: WS signed-quote responder (Black-Scholes pricing machinery
ported verbatim; quotes signed over Borsh bytes with the registered ed25519
key) and a unified venue auction bidder (covered_call / cash_secured_put /
swap; `decide_bid` ported, floor = the on-chain `options_math::min_next_bid`).
Notable correctness points: `token_recipient` on bids is the **wallet pubkey**
(settle verifies token-account owner), and `previous_bidder_refund` is `None`
when no best bid stands. Inventory bootstrap deposits into the MmAccount PDA;
non-mainnet top-ups via the gas-station faucet.

### solana-balance-monitor (ops 9012)
SOL-balance watcher for the four hot wallets (gas-station 5 SOL floor;
scheduler/keeper/mm-bot 2 SOL), `alert_id="low-balance-<service>"`, addresses
derived from the sibling services' rendered secrets files.

## Design decisions & third-party integrations

| # | Decision | Alternatives / failure points |
|---|---|---|
| 1 | **Helius** for RPC + LaserStream (indexer already committed). RPC URL is an operator secret (`options/<env>/solana-rpc`) merged into every Solana service's secrets at render time — provider swap is a secret rotation + redeploy, no code. | Helius outage stalls reads/submissions fleet-wide; Triton/QuickNode/public URLs are drop-in fallbacks. |
| 2 | **Gas sponsorship = self-hosted fee-payer co-signing** (the Octane pattern; Kora is the Foundation's productized version). Helius offers no turnkey sponsorship API. We keep our own service for per-instruction template governance and free sponsorship (no SPL-payment leg). | **Kora sidecar** if we later want KMS/TEE-held keys or paid-in-SPL sponsorship — its policy surface (program allow-list) is coarser than our templates. Hot-key drain risk bounded by the per-tx lamport cap + balance alerts. v1 rejects address-lookup-table txs (they defeat static inspection) — revisit if frontend txs outgrow static account lists. |
| 3 | **Test-token faucet moved off-chain**: mints' authority = gas-station key; `POST /faucet` (non-mainnet, per-request amounts). | On-chain faucet program later. One hot key holds two powers (sponsor + mint) — acceptable for valueless test tokens. |
| 4 | **Program deploys stay on anchor/solana CLI**; `solana-deploy` owns initialize + registry. | Sui-style in-tool publishing rejected: ids are deploy-stable; reimplementing loader plumbing adds risk, zero value. |
| 5 | **Pyth pull-oracle**: keeper posts `PriceUpdateV2` via `post_update_atomic` into persistent per-feed accounts (write-authority = keeper). | Ephemeral post+close on receiver layout drift (helper exists). Hermes outage → cranks classify Retry, vaults pause safely. A keeper restart strands ~0.007 SOL rent per feed account (new keypairs) — negligible, documented. |
| 6 | **No Solana order-book integration** (explicit scope cut): charting serves empty candles until a venue is chosen; mm-bot has no resting-order quoter; scheduler creates no pools. | When a venue lands, add one ingestion task writing `pool_trades`/`pool_mids` + broadcasting — router/DB/WS/frontend already work. |
| 7 | **Separate JWT domain** (solana-auth-service). | Merge into auth-service later if the duplication annoys; kept apart so one chain's admin compromise can't cross. |
| 8 | **Instruction encoding from the real program crates** (path deps with `no-entrypoint`) everywhere in the standalone stack; main-workspace services use hand-written borsh mirrors + discriminator checks (the solana-indexer pattern) since anchor can't enter the Sui-pinned workspace. Golden tests pin the mirrors. | IDL-generated TS-style clients — nothing exists (`app/` is empty); the crate-dep approach can't drift. |
| 9 | **Vault salt generation counter** in the scheduler (retire → bump → fresh PDA). | Without it, a paused vault's PDA collides with its replacement forever (and naive adopt-on-collision would re-adopt the paused vault). |
| 10 | **Networks**: staging *and* prod target **devnet** initially (Solana "testnet" is a validator cluster without a token ecosystem; LaserStream supports devnet). Prod→mainnet-beta later is config + secrets only. | — |

## What YOU run — Terraform (`rust-backend/infra`)

**WARNING (standing)**: this root has **local state with known destructive
drift**. `terraform plan` first, then `apply -target` ONLY the new resources.
Never blanket-apply.

New resources (both envs where applicable):

```
terraform plan   # review — expect ONLY the resources below as additions
terraform apply \
  -target='aws_ecr_repository.svc' -target='aws_ecr_lifecycle_policy.svc' \
  -target='aws_secretsmanager_secret.solana_token_info' -target='random_password.solana_token_info_db' -target='aws_secretsmanager_secret_version.solana_token_info' \
  -target='aws_secretsmanager_secret.solana_auth_service' -target='random_password.solana_auth_jwt' -target='aws_secretsmanager_secret_version.solana_auth_service' \
  -target='aws_secretsmanager_secret.solana_gas_station' -target='aws_secretsmanager_secret_version.solana_gas_station_placeholder' \
  -target='aws_secretsmanager_secret.solana_scheduler'   -target='aws_secretsmanager_secret_version.solana_scheduler_placeholder' \
  -target='aws_secretsmanager_secret.solana_keeper'      -target='aws_secretsmanager_secret_version.solana_keeper_placeholder' \
  -target='aws_secretsmanager_secret.solana_mm_bot'      -target='aws_secretsmanager_secret_version.solana_mm_bot_placeholder' \
  -target='aws_secretsmanager_secret.solana_oracle_service' -target='aws_secretsmanager_secret_version.solana_oracle_service_placeholder' \
  -target='aws_secretsmanager_secret.solana_price_charting' -target='aws_secretsmanager_secret_version.solana_price_charting_placeholder' \
  -target='aws_secretsmanager_secret.solana_rpc'         -target='aws_secretsmanager_secret_version.solana_rpc_placeholder'
```

(Resource names above match `infra/secrets.tf` as written — verify against
the plan output; the ECR targets are the existing `for_each` resources picking
up the 11 new `local.service_repos` entries.)

## What YOU run — deployment run-book

1. **Fill hand-filled secrets** (both `staging` and `prod` env paths):
   ```
   solana-keygen new -o gas-station.json     # repeat per wallet
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-gas-station \
     --secret-string '{"keypair":"<base58 or JSON array>"}'
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-scheduler --secret-string '{"keypair":"…"}'   # MUST be the program admin key
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-keeper    --secret-string '{"keypair":"…","pyth_api_key":"…"}'
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-mm-bot    --secret-string '{"keypair":"…","quote_key":"…64-hex…"}'
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-rpc      --secret-string '{"rpc_url":"https://devnet.helius-rpc.com/?api-key=…"}'
   aws secretsmanager put-secret-value --secret-id options/<env>/solana-price-charting --secret-string '{"database_url":"postgres://…"}'
   ```
   For solana-price-charting first create the `solana` database (+
   `CREATE EXTENSION timescaledb`) on the Tiger instance. **This secret must
   be filled before the first deploy** — render-secrets hard-fails on its
   placeholder (mirrors the Sui chart secret). solana-oracle-service's pyth
   key is optional (skip = anonymous tier).
2. **Deploy the programs to devnet** (once):
   `cd solana-contracts && ./scripts/deploy-devnet.sh` (or `anchor deploy`).
3. **Initialize + register** (run per env slot; staging and prod share the
   devnet cluster but keep separate JSON slots — reuse is fine initially):
   ```
   cd rust-backend
   cargo run --manifest-path tools/solana-deployment-manager/Cargo.toml -- \
     -e staging -n devnet --deploy-tokens \
     --faucet-authority <solana-gas-station pubkey> \
     -s tools/solana-deployment-manager/config/secrets.toml
   ```
   (secrets.toml holds the admin keypair; the tool README covers flags.)
   Commit the updated `solana-deployments.json`.
4. **Merge the branch**, then run **Deploy staging** with `force_all=true`
   (first deploy must seed every `.env` tag — including the new
   `SOLANA_*_TAG`s — or selective deploys reject).
5. **Provision the DBs**: run the *wipe-provision-db* workflow for
   `solana-token-info` and `solana-option-scheduler` (staging first, then
   prod with the `prod` confirmation). solana-indexer's DB flow is unchanged.
6. **Fund the wallets** (devnet: `solana airdrop 2 <pubkey> -u devnet`, or
   transfer): gas-station ≥5 SOL, scheduler/keeper/mm-bot ≥2 SOL each. Give
   the mm-bot wallet test-token inventory via the faucet or `spl-token`.
7. **Verify**: `/{env}/solana-token-info/tokens`, `/{env}/solana-indexer/health`,
   `/{env}/solana-api/buckets`, gatus board green, balance-monitor gauges
   present, and after the scheduler's first tick, buckets appear via
   `/{env}/solana-api/buckets`.
8. Prod: repeat 4–7 with **Deploy prod** (`force_all=true` first time).

Rollbacks, selective deploys, start/stop-service, and wipe-provision all work
through the existing workflows — the new services are registered in every
list (`_deploy.yml` force_all, `affected.py`, `deploy.sh`, bake, compose,
nginx, prometheus/gatus, start/stop choices — `solana-indexer` was also added
to start/stop, fixing a pre-existing gap).

## Known limitations / future work

- **Order book**: charting/mm-bot quoter gap until a Solana venue is chosen
  (decision #6).
- **Ledger admin login** unsupported by solana-auth v1 (raw `signMessage`
  only, no off-chain message envelope).
- **Settlement dust** (programs' SECURITY.md): scheduler's strike-scale
  integrality rule prevents it for scheduler-created buckets; manually
  created odd-scale buckets can still strand <1 unit of settlement — top up
  before `cleanup_bucket`.
- **Gas-station**: no auth/rate-limiting (parity with Sui — templates are the
  abuse boundary); LUT transactions rejected in v1.
- **Frontend work** is not part of this port: each frontend-facing service
  ships a `*_FRONTEND_INTEGRATION_GUIDE.md`; the biggest new frontend job is
  the execute_write flow (Ed25519 precompile ix + fresh Position keypair
  co-signer — spelled out in the quoting guide) and the fee-payer sponsorship
  flow (gas-station guide).
- The **control-panel** TUI doesn't launch the standalone-workspace binaries
  (they're outside the root cargo workspace); main-workspace Solana services
  could be registered there later if wanted.
