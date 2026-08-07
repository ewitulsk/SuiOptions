# Hybrid Exchange on Sui

Off-chain orderbook (Rust) + atomic on-chain settlement (Move), modeled on 0x
v4 Limit Orders. Implements the "Hybrid Exchange on Sui" spec (draft v0.3):
maker-signed limit orders, a trust-minimized relay, and a Move settlement
package that verifies signatures, enforces expiry/cancellation/fill
accounting, and swaps escrowed assets in a single transaction.

This directory is a self-contained product — separate from the options
protocol's `contracts/` and `rust-backend/` workspaces.

```
hybrid-exchange/
  contracts/exchange/      Move settlement package
    sources/
      order.move           Order struct, BCS decode, digest, sig verification
      balance_manager.move Per-user shared escrow (the "allowance" replacement)
      registry.move        Per-market SettlementRegistry: fills, cancels, fees
      settlement.move      fill_limit_order / match_orders / cancel / min-out
      fees.move            Fee vault sweeps
      admin.move           AdminCap (pause, fees, market listing)
    tests/                 43 tests incl. cross-language conformance vectors
  orderbook/               Rust workspace (spec §5.1)
    crates/
      core/                Domain types; Order BCS-mirrors order.move
      signing/             Consensus-critical mirror: digest + ed25519/secp256k1
                           verify + fixtures/conformance.json (release-blocking)
      book/                Per-market price-time matching engine
      router/              Multi-hop split-route planner (greedy staircase merge)
      store/               Postgres (sqlx): orders, fills, balances, cursors
      suirpc/              Thin Sui JSON-RPC client (no Sui SDK dependency tree)
      chain_sync/          Event ingestion, balance mirroring, pruning
      settlement/          match_orders submitter + abort decoding
      api/                 axum REST/WS gateway + `orderbook-service` binary
      ops/                 Config, tracing, Prometheus metrics
```

## Build & test

```sh
# Move (requires sui CLI; CI uses suiup-installed mainnet toolchain)
cd contracts/exchange && sui move test

# Rust
cd orderbook && cargo test
```

The store crate needs no database to compile (runtime sqlx queries);
migrations run automatically at service startup.

## Conformance fixtures — the consensus-critical guard

`orderbook/crates/signing/fixtures/conformance.json` pins order BCS bytes,
digests, intent-wrapped signing digests, and wallet-format signatures for
ed25519, secp256k1 (low-s) and delegated signers. The SAME vectors are
hard-coded in `contracts/exchange/tests/conformance_tests.move`. Both suites
must pass; regenerating (`cargo run -p orderbook-signing --example
gen_fixtures`) is a consensus break that voids every outstanding signature.

One deliberate deviation from spec draft v0.3 §4.3's dispatch table: the
secp256k1 path verifies over `blake2b256(intent ‖ bcs(digest))` with the
native's internal sha256 on top (i.e. `sha256(blake2b256(…))`), not over the
raw intent preimage. That is what Sui wallets/fastcrypto actually produce for
`signPersonalMessage`; both sides implement the identical recipe and the
fixtures pin it.

## Running the service

```sh
export SUI_RPC_URL=https://fullnode.mainnet.sui.io:443
export DATABASE_URL=postgres://user:pass@localhost/orderbook
export EXCHANGE_PACKAGE_ID=0x…          # published exchange package
export MARKETS_FILE=markets.json        # see markets.example.json
export RELAYER_SEED_HEX=…               # optional: enables matched mode
export ORDERBOOK_BIND=0.0.0.0:8080
export ORDERBOOK_METRICS_BIND=0.0.0.0:9184   # optional Prometheus
cargo run -p orderbook-api --bin orderbook-service
```

Without `RELAYER_SEED_HEX` the service runs open-orderbook mode only (serves
signed orders as fill tickets; no `match_orders` submission).

## API surface (spec §5.3)

- `GET  /v1/markets` — pairs, tick/lot/min sizes, registry IDs, fees
- `GET  /v1/markets/{m}/book?depth=N` — aggregated levels
- `GET  /v1/markets/{m}/orders/{digest}` — the signed order = the fill ticket
- `GET  /v1/markets/{m}/trades` — chain-event-confirmed fills
- `POST /v1/orders` — submit signed order (intake pipeline §5.4)
- `DELETE /v1/orders/{digest}` — soft cancel (signed payload; response states
  the on-chain-fillability caveat explicitly)
- `GET  /v1/accounts/{addr}/orders|fills|balance`
- `GET  /v1/routes?from=&to=&amount=` — split-route quote + PTB skeleton
- `WS   /v1/ws` — channels `book.{market}`, `trades.{market}`, `orders.{addr}`

## Trust model (spec §3/§7)

The service is trusted for liveness and fairness of matching only. It cannot
forge fills, change prices, or move funds: every fill requires the maker's
signature over exact economic terms, escrow debits happen only inside the
Move package after signature verification, and withdrawal is permissionless
and instant regardless of pause state.
