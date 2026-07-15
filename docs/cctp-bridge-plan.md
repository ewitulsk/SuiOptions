# CCTP v1 USDC Bridge: Sui ↔ Solana

> **Implementation status (2026-07-15):** all code implemented and tested —
> `cctp-contracts/` (Sui, tests pass), `solana-contracts/programs/cctp_bridge`
> (LiteSVM test passes against Circle's real devnet programs),
> `rust-backend/services/cctp-relay`, frontend `/bridge` page, gas-station
> template, `deployment-manager --deploy-cctp`, and full deployment
> registration. Remaining ops steps (need deployer keys/funds):
> 1. `deploy --deploy-cctp -e staging -n testnet` (+ mainnet later), then set
>    `cctp_bridge_package` in gas-station configs.
> 2. `anchor deploy` to devnet/mainnet (back up
>    `solana-contracts/target/deploy/cctp_bridge-keypair.json` — target/ is
>    gitignored; program id `77R21RcDcQuhWPkTNHh7BeUgBstF2Nmsysp86QpZam86`).
> 3. Create AWS secret `options/<env>/cctp-relay` = `{"sui_key", "solana_key"}`,
>    fund both relayer wallets with gas, create the `cctp_relay_<env>` DB, then
>    force-all deploy (seeds `CCTP_RELAY_TAG`).
> 4. Set `VITE_CCTP_URL` on Vercel; run the testnet e2e in Verification below.

## Context

Add a first-party USDC bridge between Sui and Solana using Circle CCTP v1 (burn on source → Circle attestation → mint on destination). Three deliverables:

1. **Contract-level entry points** — a Sui Move package and a Solana Anchor program that wrap Circle's `deposit_for_burn`, so all bridge traffic goes through our contracts (own events, future fee/control hooks).
2. **`cctp-relay` Rust microservice** — accepts `{tx_hash, origin_chain, wallet}`, stores pending transfers in Postgres, polls Circle's attestation API for ALL pending transfers, and **auto-relays** the destination-chain mint (`receiveMessage`) with service-held keys, then marks the transfer finished.
3. **Frontend** — a Bridge page to start a transfer from either chain through our contracts and watch open transfers' statuses.

Decisions already made by Evan: backend auto-relays the mint (service custodies a Sui key + Solana fee-payer); support testnet (Sui Testnet ↔ Solana Devnet) AND mainnet from day one, config-driven. Note: this bridges **Circle's real USDC coin type**, not our TUSDC test token — bridged-in USDC is not usable as options collateral on staging.

## CCTP v1 reference facts (verified from developers.circle.com)

- Domains: **Sui = 8, Solana = 5**.
- Attestation API: mainnet `https://iris-api.circle.com`, testnet `https://iris-api-sandbox.circle.com`.
  - `GET /v1/messages/{sourceDomain}/{txHash}` → message bytes + attestation + status (`pending_confirmations` | `complete`). Works with Sui tx digest and Solana signature. **This is the poll endpoint** — one call per pending transfer.
  - Rate limit 35 req/s (429 → 5-min block). Poller must stay far under this.
- Sui packages (domain 8):
  - Testnet: MessageTransmitter `0x4931e06dce648b3931f890035bd196920770e913e43e45990b383f6486fdd0a5`, TokenMessengerMinter `0x31cc14d80c175ae39777c0238f20594c6d4869cfab199f40b69f3319956b8beb`
  - Mainnet: MessageTransmitter `0x08d87d37ba49e785dde270a83f8e979605b03dc552b5548f26fdf2f49bf7ed1b`, TokenMessengerMinter `0x2aa6c5d56376c371f88a6cc42e852824994993cb9bab8d3e6450cbe3cb32b94e`
  - Source/Move deps: `circlefin/sui-cctp` (git), with `[dep-replacements]` for testnet — same pattern `contracts/Move.toml` already uses for Pyth.
- Solana programs (domain 5, same IDs on mainnet + devnet):
  - MessageTransmitter `CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd`, TokenMessengerMinter `CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3`
  - Source/IDLs: `circlefin/solana-cctp-contracts`.
- Recipient encoding: always bytes32. Sui recipient = Sui address as-is. **Solana recipient = the user's USDC associated token account (ATA)**, not the wallet — frontend/relayer must derive it, and the relayer must create it idempotently before minting.
- Receive flow: Sui destination = PTB: `message_transmitter::receive_message(message, attestation)` → Receipt hot potato → `token_messenger_minter::handle_receive_message` → complete (per Circle's sui-cctp examples). Solana destination = `receiveMessage` instruction with PDA account set from Circle's repo examples.

## Part 1 — Contracts

### 1a. Sui Move package: `cctp-contracts/` (new sibling package, NOT a module in `options_protocol`)

Separate package so `options_protocol` doesn't take a dependency on Circle's packages and the bridge can publish/upgrade independently.

- `cctp-contracts/Move.toml`: package `cctp_bridge`, edition 2024.beta; deps on `circlefin/sui-cctp` packages (message_transmitter, token_messenger_minter, stablecoin/usdc) via git, mainnet rev default + `[dep-replacements] testnet.* ` overrides (mirror the Pyth pattern in `contracts/Move.toml`).
- `cctp-contracts/sources/bridge.move`, single module `cctp_bridge::bridge`:
  - `public fun deposit_for_burn(coin: Coin<USDC>, destination_domain: u32, mint_recipient: address, tmm_state, mt_state, deny_list, treasury, ctx)` — thin wrapper: calls Circle's `token_messenger_minter::deposit_for_burn::deposit_for_burn`, captures the returned `(BurnMessage, Message)` nonce, emits our event.
  - Event `BridgeInitiated { sender, amount, destination_domain, mint_recipient, nonce }` — `has copy, drop`, emitted inline (this package is standalone; no need for the `events.move` emitter indirection).
  - `public fun` (not `entry`) per repo convention — composed in a PTB from the frontend.
- Tests: `cctp-contracts/tests/` unit test that the wrapper compiles and emits the event (Circle state objects can't be fully simulated locally; keep it a thin-wrapper compile/event test, real verification is on-chain).
- Publish: extend `rust-backend/tools/deployment-manager` with a `--deploy-cctp` path (same `publish_upgradeable` + `deployments.json` read-merge-write flow described in its README), recording `cctpBridge.packageId` per network in `rust-backend/deployments.json`. Publish to Sui testnet and mainnet.

### 1b. Solana Anchor program: `solana-contracts/programs/cctp-bridge/` (greenfield)

`solana-contracts/` has no source in the working tree (only `target/` artifacts), so this creates the Anchor workspace:

- `solana-contracts/Anchor.toml`, `Cargo.toml` (workspace), `programs/cctp-bridge/` — Anchor 0.30.x (matches the emit_cpi-style IDLs in `target/idl/`).
- Program `cctp_bridge`, one instruction `deposit_for_burn(amount: u64, destination_domain: u32, mint_recipient: [u8; 32])`:
  - CPIs into Circle TokenMessengerMinter `deposit_for_burn` (`CCTPiPYP…`). Use Anchor's `declare_program!` with Circle's vendored IDL jsons (from `circlefin/solana-cctp-contracts`) for typed CPI, or hand-built `Instruction` if the IDL route fights Anchor versions.
  - Accounts: user + user USDC token account, Circle's PDAs (sender_authority, message_transmitter, token_messenger, remote_token_messenger for domain 8, token_minter, local_token, message_sent_event_data keypair, programs).
  - `emit_cpi!(BridgeInitiated { sender, amount, destination_domain, mint_recipient })`.
- Same program ID on devnet + mainnet (vanity keypair in `target/deploy/`). Deploy with `anchor deploy` to devnet and mainnet; record IDs in the relay/frontend config.
- Tests: litesvm or `solana-program-test` with Circle programs loaded from dumped `.so` fixtures (`solana program dump`) — verifies the CPI account wiring; plus a devnet integration script under `solana-contracts/app/`.

## Part 2 — `cctp-relay` microservice

New service at `rust-backend/services/cctp-relay/`, modeled on `price-charting` (axum + diesel/r2d2 + embedded migrations + watcher loop) with `token-info`-style POST handlers.

### API (public port, e.g. 9015)

- `POST /transfers` body `{ tx_hash, origin_chain: "sui"|"solana", wallet }` → validates shape, inserts row as `pending_attestation` (idempotent on `(origin_chain, tx_hash)` unique index), 200 with the row.
- `GET /transfers?wallet=<addr>&open=true` → list transfers for the Bridge page.
- `GET /health` → "ok".
- CORS via `allowed_origins` config + `build_cors` pattern (`price-charting/src/router.rs`); `observability::middleware` metrics/trace layers.

### DB (shared RDS Postgres, `DB_HOST`/`DB_PASSWORD` pattern like indexer/token-info)

`src/db/migrations/000001_init/up.sql`:

```sql
CREATE TABLE cctp_transfers (
  id BIGSERIAL PRIMARY KEY,
  origin_chain TEXT NOT NULL,          -- 'sui' | 'solana'
  origin_tx_hash TEXT NOT NULL,
  origin_wallet TEXT NOT NULL,
  destination_wallet TEXT,             -- decoded from message once fetched
  amount NUMERIC,                      -- decoded from message
  status TEXT NOT NULL,                -- state machine below
  message_hex TEXT, attestation_hex TEXT,
  mint_tx_hash TEXT,
  error TEXT, attempts INT NOT NULL DEFAULT 0,
  -- timing instrumentation (bridge duration = minted_at - burned_at)
  burned_at TIMESTAMPTZ,               -- on-chain timestamp of the source burn tx
  attested_at TIMESTAMPTZ,             -- when the poller first saw attestation 'complete'
  minted_at TIMESTAMPTZ,               -- on-chain timestamp of the destination mint tx
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (origin_chain, origin_tx_hash)
);
CREATE INDEX ON cctp_transfers (status);
CREATE INDEX ON cctp_transfers (origin_wallet);
```

State machine: `pending_attestation → attested → minting → complete`, terminal `failed` (with `error`). `attempts` + capped exponential backoff on mint failures.

**Bridge-duration tracking** (source burn → destination mint):
- `burned_at`: on `POST /transfers` (or first poll), fetch the burn tx's **on-chain** timestamp — Sui: tx digest → checkpoint `timestamp_ms` via RPC; Solana: `getTransaction(sig).blockTime`. Uses chain time, not our ingest time, so latency between signing and POSTing doesn't skew the metric.
- `attested_at`: stamped when the poller first sees iris status `complete` (isolates Circle-attestation latency from our relay latency).
- `minted_at`: on-chain timestamp of the confirmed destination mint tx, stamped when the row reaches `complete`.
- `GET /transfers` returns all three timestamps plus a computed `duration_ms` (`minted_at - burned_at`, null while in flight).
- Emit a `cctp_bridge_duration_seconds` histogram metric (labels: `direction`, via the `metrics` crate the observability stack already scrapes) so per-direction bridge times chart in Grafana.

### Poller (`src/watcher.rs`, price-charting watcher pattern)

Every tick (~10s, `MissedTickBehavior::Skip`): load all rows in `pending_attestation`, for each call `GET {iris}/v1/messages/{domain}/{tx_hash}` (domain from `origin_chain`; reqwest workspace dep, rustls). On `complete`: store message + attestation, decode amount/recipient from message bytes, advance to `attested`. Throttle to stay well under 35 req/s; per-row errors logged and retried next tick. Poll-loop failure alert: `error!(alert_id = "cctp-attestation-poll-failed")` (only after consecutive-failure threshold).

### Relayer (`src/relayer.rs`, second loop or same tick)

For rows in `attested`:
- **Destination Sui** (Solana→Sui): build PTB `receive_message(message, attestation)` → `handle_receive_message` → complete, sign with the service's Sui key, submit via `sui-sdk` (reuse signing/submit helpers from `rust-backend/crates/sui-tx` where applicable).
- **Destination Solana** (Sui→Solana): build tx: `createAssociatedTokenAccountIdempotent(recipient ATA)` + Circle `receiveMessage` instruction (accounts per Circle's repo), sign with the service's Solana fee-payer keypair, submit via `solana-client`/`solana-sdk`.
- On submit success → `minting` with `mint_tx_hash`; confirm finality next ticks → `complete`. On failure → increment `attempts`, backoff; after N attempts → `failed` + **`error!(alert_id = "tx-failed-cctp-relay", ...)` at the relayer handler** per the tx-alerting convention (suppress benign "nonce already used" races — someone else minted — treat those as `complete`).

### Config & secrets

`config/config.toml` + `config.staging.toml` + `config.prod.toml` (loaded via `runtime_config::config_load::load_toml`, `${ENV}` expansion): iris base URL, sui rpc + package/state object IDs, solana rpc + program IDs + USDC mint, poll interval, DB url parts, `allowed_origins`. Staging/prod = testnet networks; a `network = "mainnet"` config profile carries the mainnet IDs (deployable later as its own instance without code changes). Secrets: `CCTP_RELAY_SUI_KEY`, `CCTP_RELAY_SOLANA_KEY` via AWS Secrets Manager + `rust-backend/deployment/ec2/render-secrets.sh`. Fund both relayer wallets with gas (SUI / SOL).

### Deployment registration (all 9 spots, per repo convention)

1. `rust-backend/Cargo.toml` members += `services/cctp-relay`
2. `rust-backend/Dockerfile.cctp-relay` (multi-stage, libpq5, `--config /app/config/config.${APP_ENV}.toml`)
3. `rust-backend/deployment/bake.hcl` target + default group
4. `rust-backend/infra/ecr.tf` `local.service_repos` += `"cctp-relay"` (⚠ plan + `apply -target` only — infra state has destructive drift)
5. `rust-backend/deployment/affected.py` `ALL_SERVICES` + `SERVICE_GLOBS`
6. `rust-backend/deployment/ec2/deploy.sh` ALL_SERVICES array + tag/image/health-path cases (`/staging/cctp/health`)
7. `docker-compose.staging.yml` + `docker-compose.prod.yml` service blocks
8. `rust-backend/deployment/nginx/nginx.staging.conf` + `nginx.prod.conf`: `location /staging/cctp/*` → `cctp-relay:9015`
9. `render-secrets.sh` for the two relayer keys

## Part 3 — Frontend

New Bridge page in the existing Vite+React app:

- **Route/nav**: `<Route path="/bridge">` in `frontend/src/App.tsx`; nav button in `frontend/src/components/Header.tsx` nav block (pill auto-adapts).
- **Screen**: `frontend/src/screens/Bridge.tsx` — direction toggle (Sui→Solana / Solana→Sui), amount input (`AmountInput.tsx`), destination-address field (prefilled from the connected wallet on the other chain when available), submit, and an "Open transfers" list with status badges copied from the `Activity.tsx` pattern (`pending`/`complete`/`failed`).
- **Duration display**: each open-transfer row shows a live elapsed timer (`now - burned_at`, ticking via a 1s interval) while in flight, and the final duration (`duration_ms` from the API) once complete — e.g. "✓ completed in 3m 42s".
- **Sui→Solana flow**: new PTB builder `frontend/src/tx/bridge.ts` — `coinWithBalance` USDC coin → `tx.moveCall(cctp_bridge::bridge::deposit_for_burn)` with destination domain 5 and mint_recipient = **user's Solana USDC ATA** (derive with `@solana/web3.js` `getAssociatedTokenAddressSync`, converted to bytes32 hex). Submit through `useSubmitTransaction()` (`frontend/src/tx/submit.ts`) so sponsorship works. **Add the matching PTB template to `rust-backend/crates/sui-tx/src/tx/template.rs::protocol_templates()`** (vault-loop pattern, lines ~241-255) or the gas station rejects it.
- **Solana→Sui flow**: add `@solana/web3.js` + `@solana/spl-token`; extend the existing Phantom adapter (`frontend/src/session/wallets.ts`, currently message-signing only) with `signAndSendTransaction`. Build a tx invoking our `cctp_bridge` Anchor program's `deposit_for_burn` (accounts from the program IDL) with mint_recipient = user's Sui address bytes.
- **After either submit**: `POST {VITE_CCTP_URL}/transfers` with `{tx_hash, origin_chain, wallet}` via new `frontend/src/api/bridge.ts`; new hook `frontend/src/api/useBridgeTransfers.ts` = `useQuery` polling `GET /transfers?wallet=…` with `refetchInterval: 5_000` (pattern: `useIndexerProgress.ts`).
- **Config**: `VITE_CCTP_URL` in `frontend/src/config.ts`; CCTP package/program IDs + USDC coin type/mint per network in config (testnet + mainnet maps).

## Implementation order

1. Sui `cctp-contracts` package → publish testnet → manual PTB smoke test (burn testnet USDC from Circle's faucet at faucet.circle.com, watch attestation appear in iris sandbox).
2. Solana Anchor workspace + `cctp-bridge` program → deploy devnet → devnet script smoke test.
3. `cctp-relay` service: schema + POST/GET + poller (verify a real burn reaches `attested`), then relayer for both destinations (verify mint lands and row hits `complete`).
4. Frontend Bridge page, both directions + status list; gas-station template.
5. Deployment registration + staging rollout; mainnet publishes/deploys last.

## Verification

- **Unit**: Move tests (`sui move test` in cctp-contracts); Anchor tests with dumped Circle `.so` fixtures; relay crate tests for message decoding + state transitions (mock iris responses).
- **End-to-end (testnet)**: fund a wallet with Sui testnet USDC (Circle faucet) → Bridge page Sui→Solana → confirm: row created, poller flips to `attested` (iris sandbox), relayer submits Solana mint, ATA balance increases, UI shows `complete` with a sane duration. Repeat Solana Devnet→Sui direction. Kill/restart the service mid-transfer to confirm pending rows resume from DB.
- **Timing**: after an e2e run, check `burned_at`/`attested_at`/`minted_at` are all populated and ordered, `duration_ms` matches wall-clock, and the `cctp_bridge_duration_seconds` histogram shows the sample in Prometheus.
- **Ops**: `/staging/cctp/health` green in deploy.sh; alerts verified by forcing a mint failure (empty relayer wallet) → `tx-failed-cctp-relay` fires.

## Risks / notes

- Bridged USDC ≠ TUSDC: bridged-in Sui USDC can't be used as collateral in the options protocol as-is (separate coin type). Out of scope here.
- Relayer key custody: hot wallets on both chains must stay funded with gas; balance-monitor coverage can be added later.
- Circle's testnet contracts occasionally lag mainnet revs; `[dep-replacements]` pins handle this on Sui.
- Exact Circle account lists / state object IDs get finalized during implementation from `circlefin/sui-cctp` and `circlefin/solana-cctp-contracts` examples.
