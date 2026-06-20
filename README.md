# SuiOptions

On-chain **American-style covered-call options** on the Sui blockchain, plus the automated **covered-call vaults** built on top of them and an experimental **wallet-rooted session-key** layer. This monorepo holds the Move contracts, the Rust off-chain services, the React frontend, and the AWS/Terraform infrastructure that runs it all.

The project is layered:

1. **Options Protocol** — the primary project. A pooled-bucket options primitive with FIFO exercise assignment via a monotonic cursor, quoted off-chain over an RFQ WebSocket and settled on-chain.
2. **Covered-Call Vaults** — a secondary project built on the protocol. Ribbon-style weekly vaults that sell ~0.10-delta calls through an on-chain RFQ auction, cranked permissionlessly.
3. **Session Tokens** — an experimental feature. Sign-In-With-Solana / Sign-In-With-Ethereum session keys that let a user drive the dApp without holding SUI or signing every transaction.

---

## 1. Options Protocol (primary)

📄 **Full design spec: [`options-protocol-spec.md`](./options-protocol-spec.md)**

The defining characteristic is its **pooled-bucket model with FIFO exercise assignment via a monotonic cursor**. All writers of the same `(asset, expiry, strike, settlement)` contract share a single `Bucket` shared object. Exercises advance an `exercise_cursor` in O(1); each writer's outcome is determined by where their write-range `[start, end)` falls relative to the cursor at expiry. Each option is a fungible `Coin<Call>` whose currency is generated per roll, with the bucket holding the sole `TreasuryCap` — so coin supply always equals outstanding options and bucket isolation is a type-system guarantee.

### On-chain (Sui Move) — [`contracts/`](./contracts)

| Module | Responsibility |
|--------|----------------|
| [`bucket.move`](./contracts/sources/bucket.move) | Bucket shared object, cursor logic, `execute_write` / `exercise` / `redeem` / cleanup |
| [`account.move`](./contracts/sources/account.move) | Per-user `Account` custody (asset-agnostic balances + signing key + nonces) |
| [`quote.move`](./contracts/sources/quote.move) | Signed-quote struct + Ed25519/secp signature verification + nonce tracking |
| [`position.move`](./contracts/sources/position.move) | `Position` object + redemption math |
| [`admin.move`](./contracts/sources/admin.move) · [`treasury.move`](./contracts/sources/treasury.move) | AdminCap, protocol config, fee treasury |
| [`events.move`](./contracts/sources/events.move) · [`errors.move`](./contracts/sources/errors.move) | Event types and error codes |
| [`rfq.move`](./contracts/sources/rfq.move) · [`swap_auction.move`](./contracts/sources/swap_auction.move) | On-chain RFQ auction + Pyth-bounded proceeds swap (used by the vault) |
| [`vault.move`](./contracts/sources/vault.move) · [`oracle.move`](./contracts/sources/oracle.move) | Vault rounds/shares/fees + Pyth oracle guardrails |
| `session_*.move` | `_with_session` twins of every user-facing entrypoint (see §3) |

```
cd contracts && sui move test && sui move build
```

---

## 2. Covered-Call Vaults (secondary)

📄 **Full implementation guide: [`docs/vault-implementation-guide/`](./docs/vault-implementation-guide/README.md)**

Per-asset (SUI/USDC, wBTC/USDC) weekly covered-call vaults aligned to the option-scheduler's bucket families. Users deposit underlying; at each roll the vault sells calls at the bucket nearest a 0.10-delta strike through the **on-chain RFQ**; at expiry it redeems positions and converts exercised proceeds back to underlying through an **on-chain swap auction**. Because the vault writes at bucket creation it sits at the front of the FIFO queue `[0, Q)`, so its worst case is exactly the textbook covered call — every protocol feature only improves on it. The whole round lifecycle is a **permissionless crank**.

Guide chapters:

- [01 — Contract modularization](./docs/vault-implementation-guide/01-contract-modularization.md)
- [02 — On-chain RFQ](./docs/vault-implementation-guide/02-onchain-rfq.md)
- [03 — Vault contract](./docs/vault-implementation-guide/03-vault-contract.md)
- [04 — Vault keeper](./docs/vault-implementation-guide/04-vault-keeper.md)
- [05 — Off-chain service changes](./docs/vault-implementation-guide/05-offchain-services.md)
- [06 — Backtesting engine (`vault-sim`)](./docs/vault-implementation-guide/06-vault-sim.md)
- [07 — Testing and rollout](./docs/vault-implementation-guide/07-testing-and-rollout.md)

---

## 3. Session Tokens (experimental)

📄 **Overview: [`session-tokens/README.md`](./session-tokens/README.md)** · Full design: [`sui-siws-session-key-spec.md`](./session-tokens/sui-siws-session-key-spec.md)

Wallet-rooted session keys for Sui. A user signs in **once** with a Solana (SIWS) or Ethereum (SIWE / EIP-4361) wallet, minting a scoped, expiring, revocable on-chain `SessionCap` to a browser-generated ephemeral key. That key then acts within enforced limits, with a sponsor paying gas, **without prompting for a signature on every transaction** — differentiated for the cross-chain-identity case (drive a Sui dApp without ever holding SUI).

Now **integrated into the options protocol**: `contracts/` defines `_with_session` twins of every user-facing entrypoint, the gas station sponsors the session PTB shapes, and the frontend's account dropdown owns the full session lifecycle. The TypeScript browser SDK lives at [`frontend/siws-session-sdk/`](./frontend/siws-session-sdk) (consumed from source so the app's single `npm install` resolves it). The highest-risk seam is the canonical signed message, which the Move package and SDK serialize byte-for-byte identically and pin against shared test vectors.

---

## 4. Backend services (Rust) — [`rust-backend/`](./rust-backend/README.md)

📄 **Detailed README: [`rust-backend/README.md`](./rust-backend/README.md)** (covers the core services, the `control-panel` TUI, and operator CLIs in depth)

Long-running services under [`rust-backend/services/`](./rust-backend/services):

| Service | What it does |
|---------|--------------|
| **indexer** | Tails Sui checkpoints, BCS-decodes protocol events, materializes per-account/bucket/position views, serves a GraphQL query API |
| **quoting-service** | Stateful WebSocket RFQ router between retail frontends and MM bots; authenticates MMs, validates signed quotes, tracks reservations + reputation. Holds no funds, signs nothing |
| **mm-bot** | Market-maker bot — bootstraps an Account, prices RFQs with Black-Scholes, signs and ships quotes (and bids the on-chain vault RFQ) |
| **option-scheduler** | Bucket-creation lifecycle bot; rolls a fresh vol-aware strike-grid family weekly. Holds AdminCap |
| **keeper** | Permissionless **vault keeper** — cranks the vault state machine (select strike → open/settle RFQ → redeem → swap proceeds → finalize round). Holds only a gas wallet |
| **api-service** | HTTP read layer over the indexer's GraphQL (buckets, positions, dashboard, metrics) for the frontend |
| **token-info** | Single source of truth for the supported-token catalog + on-chain deployment ids; all other services read it via `token-info-client` |
| **oracle-service** | Single internal Pyth gateway — one Hermes subscription, caches live prices + realized vol, re-broadcasts over REST + WS |
| **gas-station** | Sponsors frontend (and session) transactions: accepts PTBs, injects sponsor signature against protocol-specific templates |
| **auth-service** | JWT auth for admin access; validates signed personal-messages from configured admin wallets |
| **price-charting** | Charts DeepBook execution history into Postgres and samples vault APY metrics |
| **balance-monitor** | Watches service-wallet balances and alerts below configurable thresholds |

Supporting [`crates/`](./rust-backend/crates) (libraries): `protocol-types` (canonical BCS/JSON wire types), `sui-tx` (RPC + PTB builders + quote signing), `pricing` (Black-Scholes + greeks), `pyth-client` / `oracle-client`, `deployments` / `token-info-client` / `runtime-config` (config + on-chain id loaders), `indexer-graphql`, `api-service-client`, `auth-client`, `observability` (logs/metrics/traces), `cli-spec`, and `vault-sim` (the vault backtesting engine).

Operator [`tools/`](./rust-backend/tools): `deployment-manager` (publishes the Move package, writes `deployments.json`), `exchange` (admin CLI), `control-panel` (TUI to run/edit/tail every binary), `writer` / `trader` / `mm-quote` (test clients), `backtester` (vault Monte Carlo), `rfq-monitor`, `deepbook-pool-test`.

```
cd rust-backend && cargo check --workspace && cargo test --workspace
cargo run -p control-panel   # TUI to run everything from one screen
```

---

## 5. Frontend — [`frontend/`](./frontend/README.md)

Vite + React 18 + TypeScript dApp (Sui dapp-kit), deployed on **Vercel**, with PostHog product analytics and an "aqua"-themed CSS design system. Major areas: **Composer** (quote builder + live MM pricing), **Dashboard** (on-chain positions, exercise/claim), **Vault** (covered-call vault management), **Activity** (indexer event log), **Admin**, and **Faucet**. The session-key lifecycle (sign-in / restore / fund / withdraw / revoke) lives in `src/session/` and the account dropdown, backed by [`siws-session-sdk/`](./frontend/siws-session-sdk).

```
cd frontend && npm install && npm run dev
```

---

## 6. Infrastructure & deployment — [`rust-backend/infra/`](./rust-backend/infra/README.md)

📄 [`infra/README.md`](./rust-backend/infra/README.md) · [`infra/COST.md`](./rust-backend/infra/COST.md)

**AWS, Terraform-provisioned.** All services run as **docker-compose** on EC2 hosts (separate `staging` and `prod` hosts), fronted by an ALB (HTTPS via ACM/Route 53) that routes `/{env}/{service}` path prefixes, with nginx reverse-proxying on the host. State lives in **RDS Postgres 16**; price-charting uses an external TimescaleDB. The EC2 host doubles as a **Tailscale subnet router** for team-direct DB access. Observability is a containerized stack on the host — **Prometheus / Grafana / Tempo / Loki** — wired by the `observability` crate's `alert_id` convention.

Deploys ([`rust-backend/deployment/`](./rust-backend/deployment)): GitHub Actions runs `affected.py` to find changed services, `bake.hcl` builds per-service Docker images and pushes them to **ECR**, then `ec2/deploy.sh` pulls the affected images and runs `docker compose` on the host. `deployments.json` is bind-mounted (not baked in), so a contract redeploy doesn't require rebuilding images.

> Team VPN / Tailscale access notes: [`VPN.md`](./VPN.md).

---

## Repository map

```
options-2/
├── contracts/                  # Sui Move package (options protocol + vault + RFQ + session twins)
├── options-protocol-spec.md    # Protocol design spec (primary project)
├── docs/vault-implementation-guide/   # Covered-call vault design + build guide (secondary)
├── session-tokens/             # Experimental SIWS/SIWE session keys (Move + SDK + demo)
├── rust-backend/
│   ├── services/               # indexer, quoting, mm-bot, scheduler, keeper, api, oracle, …
│   ├── crates/                 # shared libraries (protocol-types, sui-tx, pricing, vault-sim, …)
│   ├── tools/                  # deployment-manager, exchange, control-panel, test clients
│   ├── infra/                  # Terraform (AWS: EC2, ALB, RDS, ECR, Tailscale, observability)
│   └── deployment/             # docker-compose, bake, deploy scripts, nginx, monitoring
└── frontend/                   # Vite + React dApp (Vercel) + siws-session-sdk/
```
