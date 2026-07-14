# Solana Backend Port — Architecture & Cross-Service Decisions

Scope: port every `rust-backend` service to Solana, running **alongside** the Sui
variants on the same hosts. This doc fixes the decisions every per-service guide
depends on. Per-service guides live next to this file (`NN-<service>.md`).

## Service map

| Sui service | Solana counterpart | Workspace | Port(s) | DB |
|---|---|---|---|---|
| tools/deployment-manager | tools/solana-deployment-manager (`solana-deploy`) | standalone | — | — |
| token-info | solana-token-info | main | 9005 pub / 9006 int | `solana_token_info_<env>` |
| auth-service | solana-auth-service | main | 9007 pub / 9008 int | — |
| api-service | solana-api-service | main | 9003 | — |
| quoting-service | solana-quoting-service | main | 9002 (WS+HTTP) | — |
| oracle-service | solana-oracle-service | main | 9013 | — |
| price-charting | solana-price-charting | main | 9011 | Timescale (`solana` DB on Tiger) |
| gas-station | solana-gas-station | **standalone** | 9009 | — |
| keeper | solana-keeper | **standalone** | 8086 (ops) | — |
| option-scheduler | solana-option-scheduler | **standalone** | 8087 (ops) | `solana_scheduler_<env>` |
| mm-bot | solana-mm-bot | **standalone** | 9010 (ops) | — |
| balance-monitor | solana-balance-monitor | **standalone** | 9012 (ops) | — |
| indexer | solana-indexer (already built) | standalone | 9002 / 8081 | `solana_indexer_<env>` |

Ports duplicate the Sui twins' numbers deliberately: containers are distinct on
the docker network and nginx routes by service name (`/{env}/{service}/…`), so
there is no collision. Public route names: `solana-token-info`,
`solana-auth`, `solana-api`, `solana-quoting`, `solana-charts`,
`solana-gas-station`, `solana-indexer` (existing).

## Workspace isolation rule (forced, not stylistic)

The root `rust-backend` workspace pins Sui git deps (`framework/mainnet`,
`tokio = 1.49.0`, no-tonic OTLP). Solana SDK crates need the tonic/prost stack
and their own version tree. Therefore, exactly like `services/solana-indexer`:

- **Any service that signs/submits Solana transactions or needs `solana-sdk`**
  is a **standalone cargo workspace** (`[workspace]` in its own Cargo.toml):
  solana-gas-station, solana-keeper, solana-option-scheduler, solana-mm-bot,
  solana-balance-monitor, tools/solana-deployment-manager.
- **Services that only speak HTTP/GraphQL/Postgres** stay members of the main
  workspace and reuse the shared crates normally: solana-token-info,
  solana-auth-service, solana-api-service, solana-quoting-service,
  solana-oracle-service, solana-price-charting. They handle pubkeys as base58
  `String`s (`bs58` for validation), decode on-chain accounts with plain
  `borsh` mirror structs (the solana-indexer `events.rs` pattern) — no
  `solana-sdk` anywhere in the main workspace.

Standalone workspaces path-import `crates/runtime-config` and
`crates/observability` (proven by solana-indexer) plus the new shared Solana
crates below.

## New shared crates

| Crate | Workspace | Purpose |
|---|---|---|
| `crates/solana-deployments` | main | Typed loader for `solana-deployments.json`. bs58 string ids only. Read by solana-token-info exclusively (and written by solana-deploy, which has its own writer). |
| `crates/solana-token-info-client` | main | HTTP client for solana-token-info, mirroring `token-info-client` (`Snapshot`, `fetch_blocking_until_ready`, hard cutover). Also path-importable from standalone workspaces (reqwest/serde only). |
| `crates/solana-indexer-graphql` | main | GraphQL client for solana-indexer, modeled on `crates/indexer-graphql`: `bucket(s)`, `account`, `positions*`, `auctions`, `auction_bids`, `vaults`, `vault_rounds`, `vault_apy`, `vault_receipts`, `events` scan by `sequence`, `progress`, `finalizedOnly` support. Decimal-string ints parsed at the edge. Path-importable from standalone workspaces. |
| `crates/solana-tx` | **standalone-friendly** (explicit dep versions, no `workspace = true`) | Everything Sui's `sui-tx` did: keypair loading from secrets, RPC wrapper (Helius url from shared secret, public fallback), PDA derivations, instruction building, ed25519-precompile helper for quotes, tx submit/confirm with error classification (Anchor error codes ↔ benign/retry/fatal), Pyth receiver post/close price-update helpers. |

`solana-tx` depends on the **actual program crates**
(`solana-contracts/programs/{options_core,auction_venue,options_vault}` with
`no-entrypoint`/`cpi` features) for account layouts, instruction args, event
structs, and Anchor discriminators — zero drift with the deployed programs,
same reason solana-indexer snapshots the IDLs. `crates/options_math`
(`#![no_std]`, dep-free) is path-imported wherever economic math is replicated
(keeper, mm-bot, scheduler, api-service can't — main workspace — but
options_math has no deps at all so it is importable from the main workspace
too).

## Runtime-config secrets extension

`runtime_config::Secrets` gains an optional `[solana]` section:

```toml
[solana]
devnet  = "<base58 keypair or JSON byte array>"
testnet = "..."
mainnet = "..."
default = "..."
rpc_url = "https://..."   # optional operator override (Helius keyed URL)
```

with `solana_keypair(network)` / `resolve_solana_rpc_url(network)` accessors
(parallel to `sui_private_key`). This is additive to the shared crate; Sui
services ignore it. Keypair format: accept both base58-encoded 64-byte secret
and Solana CLI JSON array; store base58 in Secrets Manager.

The shared RPC override secret is `options/<env>/solana-rpc`
(`{"rpc_url": "https://mainnet.helius-rpc.com/?api-key=…"}`), rendered by
`render-secrets.sh` into each Solana service's secrets TOML exactly like the
`sui-rpc` pattern. **Third-party decision: Helius** is the RPC provider (we
already pay for LaserStream for the indexer). Failure points: Helius outage
stalls tx submission and RPC reads for keeper/scheduler/mm-bot/gas-station —
the `rpc_url` override is a plain URL, so flipping to another provider (Triton,
QuickNode) or a public endpoint is a secret rotation, no deploy.

## solana-deployments.json

Written by `tools/solana-deployment-manager`, read only by solana-token-info.
Separate file from `deployments.json` (the Sui schema stays untouched; the two
protocols deploy independently). Same env-slot layout:

```jsonc
{
  "dev": null,
  "staging": {
    "program_info": {
      "optionsCoreProgramId": "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t",
      "auctionVenueProgramId": "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk",
      "optionsVaultProgramId": "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe",
      "configPda": "…",            // options_core Config PDA = quote protocol_id
      "treasuryPda": "…",
      "admin": "…",                // config.admin pubkey
      "network": "devnet",          // devnet | testnet | mainnet-beta
      "deployedAt": "…RFC3339…",
      "initializeSignature": "…",
      "testTokens": {               // non-mainnet only
        "TBTC": { "mint": "…", "decimals": 8, "mintAuthority": "…" },
        "TUSDC": { "mint": "…", "decimals": 6, "mintAuthority": "…" }
      }
    },
    "token_info": {
      "TBTC":  { "mint": "…", "decimals": 8, "pythFeedId": "…64-hex…" },
      "TUSDC": { "mint": "…", "decimals": 6, "pythFeedId": "…" }
    }
  },
  "prod": { … }
}
```

Notes vs Sui: program ids are **deploy-stable** (no upgradeCap / originalPackageId
concepts); `configPda`/`treasuryPda` are derivable from the program id but are
recorded anyway so no consumer needs PDA math; token identity is the **mint
address** (base58, byte-exact, no normalization ever); Pyth feed ids stay
64-hex (chain-agnostic).

## Environments and networks

Same env model as Sui: `staging` and `prod` are distinct **deployments** that
may target the same cluster. Solana network values: `devnet` initially for
both, `mainnet-beta` when prod goes live. (Solana "testnet" is a validator
test cluster with no reliable token ecosystem — devnet is the standard dev
target and what Helius LaserStream supports; the solana-indexer configs
already target devnet.)

## Quote model (off-chain contract, mirrors §on-chain)

`Quote { protocol_id: Pubkey(=Config PDA), signer_account, signer_token_recipient,
bucket, write_amount: u64, premium: u64, valid_until_ms: u64, nonce: u64 }`,
**Borsh**-encoded (not BCS). Signed ed25519 only (`signing_scheme = 0`, the only
scheme in program v1). On-chain verification is via a native
`Ed25519SigVerify` precompile instruction in the same transaction; the
quoting-service/mm-bot only need to produce/verify the detached ed25519
signature over the canonical Borsh bytes. Wire JSON keeps the Sui convention:
ints as decimal strings, ids base58.

## Event → service data-flow (identical topology to Sui)

```
programs --emit_cpi--> Helius LaserStream --> solana-indexer (GraphQL 9002)
   --> solana-api-service (JIT reads)         --> frontend REST
   --> solana-quoting-service (accounts, buckets, WriteExecuted reconcile)
   --> solana-keeper (vault/auction discovery)
   --> solana-option-scheduler (roll confirmation)
   --> solana-price-charting apy sampler
solana-token-info (9005) <-- solana-deployments.json <-- solana-deploy
solana-oracle-service (9013) <-- Pyth Hermes (SSE + Benchmarks)
```

Reorg posture per the indexer guide: UX-path consumers read confirmed-tier
views; anything folded into service-owned state that cannot be retracted
(scheduler roll confirmation, quoting reservation release, PnL-ish views) uses
`finalizedOnly: true` event scans.

## Cross-cutting conventions

- **Ids**: base58 pubkeys everywhere; `signature` not `tx_digest`; no
  canonicalization helpers — comparison is byte-exact string equality.
- **Ints on the wire**: decimal strings (same as Sui stack).
- **Milliseconds**: the programs store ms (`unix_timestamp * 1000`); all
  service math stays in ms — the Sui services' time handling ports unchanged.
- **Alerting**: identical convention — every tx-submission failure at the
  service handler: `error!(alert_id = "tx-failed-solana-<service>[-<flow>]")`,
  benign races suppressed (Anchor error codes replace Move abort codes; the
  venue's "bid too low" and "state already advanced" families are the benign
  set — enumerated per-service).
- **Metrics/health**: `observability::init`, `/health` + `/metrics` on every
  service, Prometheus scrape + Gatus checks added per service.
- **Hard cutover**: solana services crash at boot if solana-token-info is
  unreachable, exactly like the Sui stack's token-info discipline.
- **Config layout**: `config/config.toml` (dev) + `config.staging.toml` +
  `config.prod.toml` per service, `${VAR}` env expansion, `APP_ENV` selects.

## Big design decisions (logged; details in per-service guides)

1. **Gas sponsorship = self-hosted fee-payer co-signing** in solana-gas-station
   (the Kora/Octane pattern), not a third-party relayer. Helius has no
   turnkey sponsorship API; Kora (Solana Foundation's relayer node) is the
   alternative if we outgrow ours — noted in the gas-station guide.
2. **Test-token faucet moves off-chain.** No faucet program exists in
   solana-contracts; SPL mints are created by solana-deploy with mint
   authority = the gas-station's faucet key, and solana-gas-station exposes
   `POST /faucet` (non-mainnet only, per-request cap). Alternative: a tiny
   on-chain faucet program later.
3. **No order-book integration yet.** solana-price-charting ships storage +
   candle math + REST/WS + APY sampler with **no ingestion tasks**; the
   `pool_trades`/`pool_mids` tables exist but stay empty until a Solana DEX
   integration lands. mm-bot has no DeepBook-equivalent quoter; scheduler
   creates no pools.
4. **Program binary deploys stay with `anchor`/`solana` CLI** (upgrade
   authority operations are safest there); solana-deploy owns
   *initialization + registry* (initialize, test mints, catalog,
   solana-deployments.json). This diverges from the Sui tool (which publishes
   packages) because Solana program ids are stable and redeploys are
   `solana program deploy` upgrades, not new identities.
5. **Positions are fresh keypairs** (not PDAs): every service creating
   positions/receipts (keeper via vault CPIs? no — vault creates them
   internally; mm-bot/venue settle creates Position accounts) must generate
   and partially sign with ephemeral keypairs where the ix requires it.
6. **Known program quirk to operationalize**: call-bucket settlement dust
   (SECURITY.md) — scheduler picks strike scales so `slice × strike` stays
   integral, and keeper alerts (not halts) if a redeem hits the dust shortfall.
