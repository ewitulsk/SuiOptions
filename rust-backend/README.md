# rust-backend

Off-chain services for the Sui covered-call options protocol (see
`../options-protocol-spec.md`).

Two long-running services plus three CLIs / bots, all in one Cargo workspace:

- **`indexer`** (§6) — tails the Sui event stream for the protocol package
  via `sui-data-ingestion-core`, BCS-decodes events that match
  `{package_id}::events::*`, materializes per-account / per-bucket /
  per-position views in memory, and exposes the stream over a WebSocket
  fanout for the quoting service.

- **`quoting-service`** (§5) — stateful WebSocket router between retail
  frontends and market-maker bots. Authenticates MMs via a scheme-aware
  signature challenge (`ed25519` / `secp256k1` / `secp256r1`), brokers
  RFQs with a deadline window, validates signed quotes, tracks
  reservations with TTL eviction, scores MM reputation. Consumes state
  updates from the indexer; signs no transactions and holds no funds.

- **`clients`** — three binaries that drive the protocol end-to-end:
  - **`exchange`** — admin/operator CLI (create buckets, mint test tokens,
    fund accounts, set fees, withdraw treasury).
  - **`writer`** — retail-writer test client (RFQ → execute_write).
  - **`mm-bot`** — basic market-maker bot (auto-bootstraps an Account,
    prices RFQs with Black-Scholes, signs and ships Quotes).

Shared types live in **`protocol-types`** — the canonical `Quote` /
`SignedQuote` structs whose BCS encoding must byte-match the Move definition
in §3.2.7, plus the WS message envelope and indexer event types.

## Layout

```
rust-backend/
├── Cargo.toml                      # workspace
├── deployments.json                # package + AdminCap + test-token ids
├── crates/
│   ├── protocol-types/             # shared (de)serializable types
│   ├── indexer/                    # event indexer + WS fanout
│   ├── quoting-service/            # WS RFQ broker
│   ├── deployment-manager/         # `deploy` binary (publishes contracts)
│   └── clients/                    # exchange / writer / mm-bot binaries
└── tests/                          # cross-crate integration tests
```

## Build & test

```
cargo check --workspace
cargo test --workspace
```

## Run locally

Each service loads a TOML config from `CONFIG_PATH` (default
`config/testnet.toml` resolved against the crate dir). Edit
`crates/<service>/config/testnet.toml` or point `CONFIG_PATH` at your own.

The indexer resolves the deployed `package_id` at startup from
`deployments.json` using the `network` field in its TOML — a redeploy
only requires updating `deployments.json` (the `deployment-manager`
writes it automatically), no indexer config edit.

Terminal 1 — indexer (tails Sui checkpoints, serves WS fanout):

```
cargo run -p indexer
```

Terminal 2 — quoting service (subscribes to the indexer over WS):

```
cargo run -p quoting-service
```

Both honor `RUST_LOG` (e.g. `RUST_LOG=info,quoting_service=debug`).

## Clients

All three binaries live in `crates/clients` and resolve every chain-side
id (package, AdminCap, ProtocolConfig, Treasury, the test-tokens package
and its per-symbol Faucets) from `deployments.json`. Nothing about tokens
or addresses is hardcoded in the binaries — re-run `deploy` on a fresh
network, update `deployments.json`, and the clients follow.

### Secrets

Every binary that signs anything (`deploy`, `exchange`, `writer`,
`mm-bot`) reads its keys from a single TOML file — `secrets.toml` in the
working directory by default, or `--secrets <path>` to override. There
is **no environment-variable fallback**: if a key is missing, the binary
refuses to start.

Copy the committed template and fill in real keys:

```bash
cp secrets.example.toml secrets.toml
$EDITOR secrets.toml
```

Shape:

```toml
[sui]
# Per-network Sui keys (Sui's `suiprivkey1…` bech32 format).
testnet = "suiprivkey1..."
# mainnet = "suiprivkey1..."
# devnet  = "suiprivkey1..."
# Optional shared fallback for any per-network slot left blank.
# default = "suiprivkey1..."

[mm_bot]
# Two formats accepted for the MM quote secret:
# 1. Sui bech32 keypair (`suiprivkey1…`, e.g. `sui keytool export`). The
#    encoded scheme must match mm-bot.toml's `signing_scheme`.
# 2. Raw 32-byte hex (`0x…` prefix optional). Interpreted per
#    mm-bot.toml's `signing_scheme` (Ed25519 seed / k1 scalar / r1 scalar).
quote_key = "suiprivkey1..."
# or: quote_key = "0xabcdef..."
```

`secrets.toml` is in `.gitignore`; `secrets.example.toml` is committed.

### Building

```
cargo build --release -p clients         # builds exchange, writer, mm-bot
# or run directly without a release build:
cargo run --release -p clients --bin exchange -- <args>
```

The first build pulls a chunk of the Sui workspace and takes ~5 min;
incremental builds are seconds.

---

### `exchange` — admin / operator CLI

Drives every AdminCap-gated entrypoint in the protocol plus the test-token
faucets. Admin-only commands (`create-buckets`, `set-fee`,
`withdraw-treasury`) refuse to run unless the signer matches the deployer
recorded in `deployments.json`; the rest (`mint`, `fund-account`, `info`)
work for any signer.

**Global flags** (apply to every subcommand):

| Flag | Default | What it is |
|------|---------|------------|
| `-d, --deployments <path>` | `deployments.json` | Where to read deployment ids from. |
| `-n, --network <mainnet\|testnet\|devnet>` | `testnet` | Network slot inside `deployments.json`. |
| `--gas-budget <mist>` | `200000000` | Per-transaction gas cap. |

**Subcommands.**

`info` — dump every resolvable id (package, AdminCap, ProtocolConfig,
treasury, derived `protocol_id` bytes, deployer, signer address, and the
test-token table). Use this first to verify your env is wired up:

```
cargo run --release -p clients --bin exchange -- info
```

`create-buckets` — call `bucket::new_call_option<U, S>`. Creates `count`
shared buckets at strikes `start_strike + i * strike_interval`:

```
cargo run --release -p clients --bin exchange -- create-buckets \
  --underlying TBTC                       \
  --settlement TUSDC                      \
  --expiry-ms     1769443200000           \
  --start-strike  500                     \
  --strike-interval 50                    \
  --count         4
```

`--underlying` / `--settlement` accept either a symbol from
`deployments.testTokens` (`TBTC`, `TDEEP`, `TUSDC`, `TWAL`) or a fully
qualified `0x…::module::Type` string. `--expiry-ms` is a Sui clock
timestamp.

**Strike units** (this trips everyone up the first time): the on-chain
`strike` is **settlement smallest-units per underlying smallest-unit**,
not per-coin. For BTC at $50 000 with TBTC (8 decimals) and TUSDC
(6 decimals):

```
strike = USD/BTC × 10^(settlement_decimals) / 10^(underlying_decimals)
       = 50_000 × 10^6 / 10^8 = 500
```

So `--start-strike 500 --strike-interval 50 --count 4` makes buckets at
strikes $50 000 / $55 000 / $60 000 / $65 000 per BTC. Make the MM bot's
`spot_price` in `mm-bot.toml` use the same convention.

`mint` — faucet-mint `--amount` of `--token` to the signer:

```
cargo run --release -p clients --bin exchange -- mint \
  --token TUSDC --amount 1000000000
```

`fund-account` — mint a test token and deposit it into an Account in one
PTB. Useful for topping up an MM Account or seeding a new tester:

```
cargo run --release -p clients --bin exchange -- fund-account \
  --account 0xabc...        \
  --token   TUSDC           \
  --amount  1000000000000
```

`set-fee` — `admin::set_fee_bps`. Capped at 1000 bps on chain:

```
cargo run --release -p clients --bin exchange -- set-fee --bps 50
```

`withdraw-treasury` — `treasury::withdraw<T>`. `--token` accepts a symbol
or a Move type:

```
cargo run --release -p clients --bin exchange -- withdraw-treasury \
  --token TUSDC --amount 1000000 --recipient 0xabc...
```

---

### `writer` — retail-writer test client

Walks the full §8.1 writer flow:

1. Opens a WS to the quoting service as `RetailRole::Writer`.
2. Sends an `RFQRequest` for `(bucket, write_amount, side=Writer)`.
3. Picks the top quote (the service has already sorted by best premium).
4. Submits one PTB that mints the underlying via the test-token faucet,
   constructs the `Quote` + `SignedQuote` inline, and calls
   `bucket::execute_write<U, S>`. The position NFT lands in the writer's
   wallet; the MM gets the call-option NFT.

```
cargo run --release -p clients --bin writer -- \
  --bucket 0xBUCKET_ID                         \
  --write-amount 100000                        \
  --underlying TBTC                            \
  --settlement TUSDC
```

| Flag | Default | What it is |
|------|---------|------------|
| `-b, --bucket <id>` | required | Bucket id (output of `exchange create-buckets`). |
| `-w, --write-amount <u64>` | required | Underlying amount, raw smallest-units. TBTC has 8 decimals, so `100000` = 0.001 TBTC. |
| `--underlying <SYMBOL>` | `TBTC` | Symbol resolved against `deployments.testTokens`. |
| `--settlement <SYMBOL>` | `TUSDC` | Same. |
| `-q, --quoting-url <ws>` | `ws://127.0.0.1:9002/` | Quoting service WS endpoint. |
| `--rfq-timeout-secs <n>` | `5` | How long to wait for the RFQResponse. |
| `--gas-budget <mist>` | `200000000` | Per-tx gas cap. |
| `-n, --network` | `testnet` | Network slot. |
| `-d, --deployments <path>` | `deployments.json` | Deployments file. |

Output ends with `✓ execute_write digest: 0x…`. The writer keeps the
Position NFT (redeemable post-expiry) and receives the net premium as a
fresh coin in their wallet.

---

### `mm-bot` — market-maker bot

Two phases per run.

**Bootstrap** (once, on first start when no `mm-bot.account.json` is
present):

1. Reads `signing_scheme` from the TOML and derives the verifying key from
   the `MM_QUOTE_KEY` secret accordingly (Ed25519 seed / Secp256k1 scalar
   / Secp256r1 scalar — all 32 bytes).
2. Calls `account::create_and_share_account(scheme, signing_pubkey)` to
   create a shared Account whose registered `(scheme, pubkey)` matches.
3. Mints `bootstrap_settlement_amount` of the settlement token from its
   faucet and deposits it into the new Account in one PTB so the bot can
   pay premiums on day one.
4. Persists `(account_id, settlement_symbol)` to `mm-bot.account.json`.

**Serve** (every run after that):

1. Authenticates with the quoting service via signature challenge-response
   using the configured scheme (Ed25519 signs raw challenge bytes; the
   ECDSA schemes sign the SHA-256 digest, matching the contract's
   `hash = 1` flag).
2. Loops on `RFQBroadcast` frames. Prices each option with Black-Scholes
   using the configured `spot_price`, `vol`, `rate`, `days_to_expiry`.
3. BCS-encodes the resulting `Quote`, signs with the scheme-appropriate
   algorithm, sends the signed Quote back. Pongs on Ping; ignores
   `AccountStateUpdate` / `ReservationConfirmed` / `ReservationReleased`
   (observed only for MVP).

**Required secrets** (in `secrets.toml`):

```toml
[sui]
testnet = "suiprivkey1..."   # pays gas for the bootstrap tx

[mm_bot]
# Either a Sui bech32 keypair (scheme baked in, must match
# mm-bot.toml's `signing_scheme`)…
quote_key = "suiprivkey1..."
# …or raw 32-byte hex, interpreted per `signing_scheme`:
# quote_key = "0xabcdef..."
```

**Config** (`crates/clients/config/mm-bot.toml`):

```toml
quoting_url        = "ws://127.0.0.1:9002/"

# Quote-signing scheme. Must match what `account::create_and_share_account`
# registered on chain. One of `ed25519` / `secp256k1` / `secp256r1`.
signing_scheme     = "ed25519"

underlying_symbol  = "TBTC"
settlement_symbol  = "TUSDC"

# Black-Scholes inputs. spot_price is in settlement-asset smallest-units
# (TUSDC has 6 decimals → 50_000 USD = 50_000_000_000).
spot_price         = 50_000_000_000
vol                = 0.6
rate               = 0.05
days_to_expiry     = 30.0
quote_ttl_ms       = 30_000

roles = ["trader_mm", "writer_mm"]

# Mint+deposit this much settlement into the freshly created Account on
# first run so it can immediately pay premiums.
bootstrap_settlement_amount = 1_000_000_000_000

# token_recipient = "0x..."  # optional; defaults to the bot's Sui address
```

**Run:**

```
cargo run --release -p clients --bin mm-bot -- \
  --config crates/clients/config/mm-bot.toml \
  --account-state mm-bot.account.json
```

The bot logs its account id on bootstrap; that's what the quoting service
and the writer will see in `RFQResponse.quotes[].mm_id`.

---

### End-to-end demo (testnet)

Five terminals walk through one writer transaction from cold start.

```bash
cd rust-backend
# One-time secret setup — fill in your Sui testnet bech32 key and a
# fresh 32-byte hex value for `mm_bot.quote_key`.
cp secrets.example.toml secrets.toml
$EDITOR secrets.toml
```

`secrets.toml` is read by every signing binary; no env vars needed.

**T1 — indexer** (tails Sui checkpoints, fans out events over WS):

```bash
cargo run --release -p indexer
```

**T2 — quoting service** (subscribes to the indexer):

```bash
cargo run --release -p quoting-service
```

**T3 — operator: verify wiring and create a bucket** (one-shot):

```bash
cargo run --release -p clients --bin exchange -- info
cargo run --release -p clients --bin exchange -- create-buckets \
  --underlying TBTC --settlement TUSDC \
  --expiry-ms 1769443200000 \
  --start-strike 500 \
  --strike-interval 50 \
  --count 1
# Note the printed bucket id. Strike of 500 = $50k/BTC for the
# TBTC (8 dec) / TUSDC (6 dec) pair — see the "Strike units" note above.
```

**T4 — MM bot** (persistent; first run bootstraps + funds its Account.
Make sure `mm_bot.quote_key` in `secrets.toml` is set. For Ed25519 any
32 random bytes work; for the ECDSA schemes the scalar must be < curve
order):

```bash
cargo run --release -p clients --bin mm-bot
```

**T5 — writer** (one shot per option you write):

```bash
cargo run --release -p clients --bin writer -- \
  --bucket 0xBUCKET_ID_FROM_T3 \
  --write-amount 100000
```

The writer prints `✓ execute_write digest: …`. The position NFT is now
in the writer's wallet; the MM bot has a `CallOption` NFT and its
Account's TUSDC balance has been debited by the premium it quoted.
