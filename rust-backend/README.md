# rust-backend

Off-chain services for the Sui covered-call options protocol (see
`../options-protocol-spec.md`).

Organized into three buckets:

- **`services/`** — long-running processes.
  - **`indexer`** (§6) — tails Sui's checkpoint stream via
    `sui-data-ingestion-core`, BCS-decodes `{package_id}::events::*`,
    materializes per-account / per-bucket / per-position views in memory,
    and exposes the stream over a WebSocket fanout for the quoting service.
  - **`quoting-service`** (§5) — stateful WebSocket router between retail
    frontends and market-maker bots. Authenticates MMs via a scheme-aware
    signature challenge (`ed25519` / `secp256k1` / `secp256r1`), brokers
    RFQs with a deadline window, validates signed quotes, tracks
    reservations with TTL eviction, scores MM reputation. Consumes state
    updates from the indexer; signs no transactions and holds no funds.
  - **`mm-bot`** — basic market-maker bot (auto-bootstraps an Account,
    prices RFQs with Black-Scholes, signs and ships Quotes).
  - **`option-scheduler`** — bucket-creation lifecycle bot. Tracks live
    bucket families per `(underlying, settlement)` pair via the indexer
    and rolls a fresh family (`bucket::new_call_option`) when the latest
    is within a configurable roll-threshold of expiry. Holds AdminCap;
    replaces manually running `exchange create-buckets` on a cadence.

- **`tools/`** — one-shot operator binaries.
  - **`deployment-manager`** (`deploy` binary) — compiles and publishes the
    options-protocol Move package; records every important on-chain id
    into `deployments.json`.
  - **`exchange`** — admin/operator CLI (create buckets, mint test tokens,
    fund accounts, set fees, withdraw treasury).
  - **`writer`** — retail-writer test client (RFQ → `execute_write`).
  - **`control-panel`** (`control` binary) — single-screen TUI that
    start/stop/restarts every service, tool, and client; live-tails their
    stdout/stderr; and edits their CLI flags inline. Flag metadata flows
    directly from each binary's `clap` derive via the
    `shared::define_program!` macro, so there's no second source of truth
    to drift.

- **`shared/`** — single library crate. BCS-canonical `Quote` /
  `SignedQuote` (which must byte-match §3.2.7 of the spec), the WS
  message envelope, indexer event mirrors, the `Deployments` /
  `Secrets` loaders, Black-Scholes pricing, scheme-aware quote signing,
  Sui client + PTB builders.

## Layout

```
rust-backend/
├── Cargo.toml                      # workspace
├── deployments.json                # package + AdminCap + test-token ids
├── secrets.example.toml            # committed template; per-binary copies are gitignored
├── shared/                         # library crate (protocol_types, deployments, secrets, …)
├── services/
│   ├── indexer/          config/{config.toml}
│   ├── quoting-service/  config/{config.toml}
│   ├── mm-bot/           config/{config.toml, secrets.toml, mm-bot.account.json}
│   └── option-scheduler/ config/{config.toml, secrets.toml}
├── tools/
│   ├── deployment-manager/  config/{secrets.toml}
│   ├── exchange/            config/{secrets.toml}
│   ├── writer/              config/{secrets.toml}
│   └── control-panel/       # TUI: start/stop every binary, edit flags, tail logs
└── tests/                          # cross-crate integration tests
```

Every binary that needs a config or a secrets file has its **own copy**
under that binary's `config/` directory — even when two binaries share
the same Sui key. CLI flags (`-c/--config`, `-s/--secrets`) override the
per-binary defaults, but there is no env-var fallback for secrets.

## Build & test

```
cargo check --workspace
cargo test --workspace
```

## Run locally

Each binary picks up its own config:

| Binary | Config | Secrets |
|---|---|---|
| `indexer` | `services/indexer/config/config.toml` | — (read-only on chain) |
| `quoting-service` | `services/quoting-service/config/config.toml` | — (signs nothing) |
| `mm-bot` | `services/mm-bot/config/config.toml` | `services/mm-bot/config/secrets.toml` |
| `option-scheduler` | `services/option-scheduler/config/config.toml` | `services/option-scheduler/config/secrets.toml` |
| `deploy` | (CLI flags only) | `tools/deployment-manager/config/secrets.toml` |
| `exchange` | (CLI flags only) | `tools/exchange/config/secrets.toml` |
| `writer` | (CLI flags only) | `tools/writer/config/secrets.toml` |

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

## Control panel TUI

Instead of juggling five terminals, run everything from a single TUI:

```
cargo run -p control-panel
```

The binary is named `control`, so once built you can also invoke it as
`./target/debug/control` (or `./target/release/control`). It must be run
from the workspace root (`rust-backend/`) — `cargo run` handles that
automatically.

### Layout

```
┌─Programs─────────────┬─ <selected program> ──────────────────────────┐
│ ▶ indexer       RUN  │  indexer       RUNNING  pid=12345              │
│   quoting-svc        │  cwd: .                                        │
│   mm-bot     CRASHED │  $ cargo run -p indexer -- --config ...        │
│   exchange           │  ─────────────────────────────────────────────│
│   writer             │  │ Flags │ Logs │ Docs │                      │
│   deploy             │  ─────────────────────────────────────────────│
│                      │   --config           services/indexer/...      │
│                      │   (more flags…)                                │
└──────────────────────┴────────────────────────────────────────────────┘
 [↑/↓] program  [→/←] panel  [Tab] tab  [j/k] flag  [e] edit  [r] run  [q] quit
```

- **Programs** (left) — every binary in the workspace plus its current
  status (`STOPPED` / `RUNNING (pid=…)` / `DONE` / `CRASHED`).
- **Header** — shows the exact `cargo run …` command that will be
  invoked, including every overridden flag. Nothing is hidden.
- **Flags tab** — every flag from the binary's `clap` derive, with
  defaults, possible values (for `value_enum`), and one-line help. Edited
  values are highlighted yellow. For programs with subcommands
  (`exchange`), press `c` to pick which one to run.
- **Logs tab** — live tail of the child process's stdout (white) and
  stderr (red). `j` / `k` to scroll, `g` / `G` for top / bottom.
- **Docs tab** — the long-form `#[command(about = …)]` and every flag's
  help text.

### Keybindings

| Key | What it does |
|---|---|
| `↑` / `↓` | Move selection in the program list |
| `←` / `→` | Switch focus between program list and content pane |
| `Tab` / `Shift+Tab` | Cycle Flags / Logs / Docs |
| `j` / `k` | Move within the focused list (flag row, log line) |
| `e` or `Enter` | Edit the selected flag (`Enter` commits, `Esc` cancels). For bool flags, toggles directly. |
| `Space` | Toggle bool flag or cycle through `possible_values` |
| `R` | Revert the selected flag to its clap default |
| `c` | Pick subcommand (programs that have them — `exchange`) |
| `r` | Run the selected program |
| `s` | Stop the selected program (SIGTERM) |
| `Shift+R` | Restart (stop then run) |
| `q` or `Ctrl+C` | Quit (kills any still-running children) |

### Adding a new program

Every binary registers itself once via one macro call in its `lib.rs`:

```rust
#[derive(clap::Parser)]
pub struct Cli {
    /// Help text shows up in the TUI's Docs tab and as the flag's hint.
    #[arg(short, long, default_value = "deployments.json")]
    pub deployments: std::path::PathBuf,

    // …
}

shared::define_program! {
    id          = "my-tool",
    cargo_pkg   = "my-tool",              // matches `cargo run -p …`
    working_dir = ".",                    // relative to the workspace root
    description = "One-line summary shown in the TUI's program list.",
    cli         = crate::Cli,
}
```

That generates a `program_spec()` function which wraps the clap
`CommandFactory`. Then add it to
`tools/control-panel/src/programs.rs::all()` so the TUI knows about it:

```rust
pub fn all() -> Vec<ProgramSpec> {
    vec![
        // …
        my_tool::program_spec(),
    ]
}
```

Defaults, help text, `value_enum` variants, and subcommands all come
from the clap derive — there's nothing to keep in sync between the binary
and the TUI.

## Clients

The three clients (`exchange`, `writer`, `mm-bot`) and the
`deployment-manager`'s `deploy` binary all resolve every chain-side id
(package, AdminCap, ProtocolConfig, Treasury, the test-tokens package
and its per-symbol Faucets) from `deployments.json`. Re-run `deploy` on
a fresh network, update `deployments.json`, and everything else follows.

### Secrets

Every binary that signs anything (`deploy`, `exchange`, `writer`,
`mm-bot`) reads its keys from its own `config/secrets.toml`. There is
**no environment-variable fallback**: if a key is missing, the binary
refuses to start.

Workspace bootstrap:

```bash
# One template at the workspace root; copy it into each binary's
# config/ dir and fill in real keys. The same key can appear in
# multiple files — each binary just reads its own.
cp secrets.example.toml services/mm-bot/config/secrets.toml
cp secrets.example.toml tools/deployment-manager/config/secrets.toml
cp secrets.example.toml tools/exchange/config/secrets.toml
cp secrets.example.toml tools/writer/config/secrets.toml
```

Shape of any `secrets.toml`:

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

`config/secrets.toml` is gitignored everywhere; `secrets.example.toml`
is the only committed copy.

### Building

```
# Build every binary in the workspace.
cargo build --release --workspace
# Or build a single one:
cargo build --release -p exchange
# Or run directly:
cargo run --release -p exchange -- <args>
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
cargo run --release -p exchange -- info
```

`create-buckets` — call `bucket::new_call_option<U, S>`. Creates `count`
shared buckets at strikes `start_strike + i * strike_interval`:

```
cargo run --release -p exchange -- create-buckets \
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
cargo run --release -p exchange -- mint \
  --token TUSDC --amount 1000000000
```

`fund-account` — mint a test token and deposit it into an Account in one
PTB. Useful for topping up an MM Account or seeding a new tester:

```
cargo run --release -p exchange -- fund-account \
  --account 0xabc...        \
  --token   TUSDC           \
  --amount  1000000000000
```

`set-fee` — `admin::set_fee_bps`. Capped at 1000 bps on chain:

```
cargo run --release -p exchange -- set-fee --bps 50
```

`withdraw-treasury` — `treasury::withdraw<T>`. `--token` accepts a symbol
or a Move type:

```
cargo run --release -p exchange -- withdraw-treasury \
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
cargo run --release -p writer -- \
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

**Config** (`services/mm-bot/config/config.toml`):

```toml
quoting_url        = "ws://127.0.0.1:9002/"

# Quote-signing scheme. Must match what `account::create_and_share_account`
# registered on chain. One of `ed25519` / `secp256k1` / `secp256r1`.
signing_scheme     = "ed25519"

underlying_symbol  = "TBTC"
settlement_symbol  = "TUSDC"

# spot_price is in the same units as the bucket's on-chain `strike`:
# settlement smallest-units per underlying smallest-unit. For BTC at $50k
# with TBTC (8 dec) and TUSDC (6 dec): 50_000 × 10^6 / 10^8 = 500.
spot_price         = 500
vol                = 0.6
rate               = 0.05
quote_ttl_ms       = 30_000

roles = ["trader_mm", "writer_mm"]

# Mint+deposit this much settlement into the freshly created Account on
# first run so it can immediately pay premiums.
bootstrap_settlement_amount = 1_000_000_000_000

# token_recipient = "0x..."  # optional; defaults to the bot's Sui address
```

**Run:**

```
cargo run --release -p mm-bot
```

Defaults point at `services/mm-bot/config/{config,secrets}.toml` and
persist Account state at `services/mm-bot/config/mm-bot.account.json`.
Override any with `--config` / `--secrets` / `--account-state`.

The bot logs its account id on bootstrap; that's what the quoting service
and the writer will see in `RFQResponse.quotes[].mm_id`.

---

### `option-scheduler` — bucket-creation lifecycle bot

Owns the protocol's bucket-creation cadence. For every configured
`(underlying, settlement)` pair, the scheduler tracks which bucket
families (a family = N strikes sharing one `expiry_ms`) currently exist
on chain, and submits `bucket::new_call_option<U, S>` when the latest
family is closer than `roll_threshold_ms` to its expiry. The `exchange
create-buckets` subcommand stays as the manual/break-glass tool; the
scheduler replaces the human cron job behind it.

**Boot flow:**

1. Loads `deployments.json` + `secrets.toml`. Refuses to start if the
   configured signer is not the deployer recorded in `deployments.json`
   — only that address holds AdminCap, so any other signer would just
   make the chain revert every tick.
2. Connects to the indexer's WS fanout (`indexer_url`) with
   `Subscribe { after_sequence: 0 }`. The snapshot rebuilds the
   in-memory family registry; from then on the bot follows the live
   stream forever. Buckets it submits itself flow back through the
   indexer the same way, so the registry stays single-sourced.
3. Starts a tick loop (`tick_secs`, default 60s).

**Tick (per pair):**

- If the latest family's expiry is more than `roll_threshold_ms` away, do
  nothing.
- Otherwise compute `next_expiry = latest_expiry + expiry_interval_ms`
  (cold start aligns to the next epoch-aligned multiple of
  `expiry_interval_ms`).
- Compute the strike grid from the configured spot, `strikes_below`,
  `strikes_above`, `interval_pct`. The grid is converted to chain
  units using the same `spot × 10^settlement_decimals /
  10^underlying_decimals` rule the MM bot uses (see the "Strike units"
  note in the `exchange create-buckets` section).
- Call `shared::tx::admin::new_call_option<U, S>(...)`. Log the digest
  and every `Bucket` id from `ObjectChanges`. Don't touch the registry
  directly — wait for the indexer's `BucketCreated` events to land.

**`--dry-run`:** logs the exact `NewCallOptionArgs` it would submit and
skips `execute_transaction_block`. Run this on the first deployment to
eyeball the strike grid before letting the bot near AdminCap.

**Config** (`services/option-scheduler/config/config.toml`):

```toml
indexer_url       = "ws://127.0.0.1:9001/"
tick_secs         = 60
roll_threshold_ms = 604_800_000        # 1 week

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604_800_000      # weekly cadence
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0
  [pairs.spot]
  source = "static"
  usd    = 50_000.0
```

`spot.source = "static"` is the only source for MVP — a future `http`
source (Binance / Coingecko / Pyth) is documented in the ticket but
out of scope here.

**Run:**

```bash
# Dry-run first to inspect the registry and the strike grid for each
# pair near roll-threshold. No on-chain calls.
cargo run --release -p option-scheduler -- --dry-run

# Live:
cargo run --release -p option-scheduler
```

**Security note.** The scheduler runs with AdminCap — the most
privileged key in the protocol (§9.1 of the spec: "Compromise of
AdminCap is catastrophic for new bucket creation and fee setting but
cannot directly drain user funds"). Mitigations baked into MVP:

- Per-binary `secrets.toml`, gitignored, no env-var fallback.
- The scheduler only calls one AdminCap-gated entrypoint:
  `new_call_option`. `set_fee_bps` and `withdraw_treasury` are *not*
  exposed; use the `exchange` CLI for those.
- Pre-mainnet: rotate AdminCap to a multisig and re-run the scheduler
  with a co-signer flow (out of scope here — file as a follow-up).

**Out of scope (file follow-ups):**

- `cleanup_bucket` automation for drained, post-expiry buckets — needs
  a new `shared::tx::admin::cleanup_bucket` builder first.
- Real spot oracle integration (Pyth / Switchboard).
- Non-test-token underlyings (production CoinType plumbed in directly
  by fully-qualified Move type instead of a symbol lookup).

---

### End-to-end demo (testnet)

Five terminals walk through one writer transaction from cold start.

```bash
cd rust-backend
# One-time secret setup — copy the template into each signing binary's
# config/ dir, then fill in real keys.
for d in services/mm-bot tools/deployment-manager tools/exchange tools/writer; do
  cp secrets.example.toml "$d/config/secrets.toml"
done
$EDITOR services/mm-bot/config/secrets.toml \
        tools/deployment-manager/config/secrets.toml \
        tools/exchange/config/secrets.toml \
        tools/writer/config/secrets.toml
```

Per-binary secrets — no env vars anywhere.

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
cargo run --release -p exchange -- info
cargo run --release -p exchange -- create-buckets \
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
cargo run --release -p mm-bot
```

**T5 — writer** (one shot per option you write):

```bash
cargo run --release -p writer -- \
  --bucket 0xBUCKET_ID_FROM_T3 \
  --write-amount 100000
```

The writer prints `✓ execute_write digest: …`. The position NFT is now
in the writer's wallet; the MM bot has a `CallOption` NFT and its
Account's TUSDC balance has been debited by the premium it quoted.
