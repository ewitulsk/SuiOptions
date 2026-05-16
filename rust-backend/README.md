# rust-backend

Off-chain services for the Sui covered-call options protocol (see
`../options-protocol-spec.md`).

Two services, both written in Rust, organized as a Cargo workspace:

- **`indexer`** (§6) — tails the Sui event stream for the protocol package
  via `sui-data-ingestion-core`, BCS-decodes events that match
  `{package_id}::events::*`, materializes per-account / per-bucket /
  per-position views in memory, and exposes the stream over a WebSocket
  fanout for the quoting service.

- **`quoting-service`** (§5) — stateful WebSocket router between retail
  frontends and market-maker bots. Authenticates MMs via Ed25519 challenge,
  brokers RFQs with a deadline window, validates signed quotes, tracks
  reservations with TTL eviction, scores MM reputation. Consumes state updates
  from the indexer; signs no transactions and holds no funds.

Shared types live in **`protocol-types`** — the canonical `Quote` /
`SignedQuote` structs whose BCS encoding must byte-match the Move definition
in §3.2.7, plus the WS message envelope and indexer event types.

## Layout

```
rust-backend/
├── Cargo.toml                      # workspace
├── crates/
│   ├── protocol-types/             # shared (de)serializable types
│   ├── indexer/                    # event indexer + WS fanout
│   └── quoting-service/            # WS RFQ broker
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
The indexer's `package_id` must match the deployed `options_protocol`
package once the contracts ship; until then the indexer runs but matches
no events.

Terminal 1 — indexer (tails Sui checkpoints, serves WS fanout):

```
cargo run -p indexer
```

Terminal 2 — quoting service (subscribes to the indexer over WS):

```
cargo run -p quoting-service
```

Both honor `RUST_LOG` (e.g. `RUST_LOG=info,quoting_service=debug`).
