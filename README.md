# SuiOptions

On-chain **American-style covered-call options** on the Sui blockchain: a pooled-bucket options primitive with FIFO exercise assignment, quoted off-chain over an RFQ WebSocket and settled on-chain. This repo holds the Move contracts, the Rust off-chain services that route and index the protocol, and the React frontend.

📄 **Full design spec: [`docs/options-protocol-spec.md`](./docs/options-protocol-spec.md)**

## The core mechanism

The protocol's defining characteristic is its **pooled-bucket model with FIFO exercise assignment via a monotonic cursor**. All writers of the same `(asset, expiry, strike, settlement)` contract share a single `Bucket` shared object with two monotonic counters, `total_written` and `exercise_cursor`. Each write occupies a contiguous range `[start, end)` on an unbounded number line; each exercise advances the cursor in O(1) without touching any position. At redemption, a position's outcome is determined entirely by where its range falls relative to the final cursor — mathematically equivalent to FIFO assignment, with O(1) state changes per exercise.

This preserves the protocol's core economic property: early writers (who received lower premiums, when the option was less in-the-money) face exercise first; late writers (higher premiums) sit deeper in the queue. Exercise exposure corresponds to premium received.

Each option is a fungible **`Coin<Call>`** whose One-Time-Witness currency is generated per roll, with the bucket holding the sole `TreasuryCap` — so outstanding coin supply always equals outstanding options, and bucket isolation is a type-system guarantee rather than a runtime check. Holders get native coin semantics (wallet balances, `split`/`join`) for free.

Two symmetric RFQ flows serve retail on both sides — writers selling covered calls to trader MMs, and traders buying covered calls from writer MMs — through a single unified on-chain entry point driven by MM-signed quotes (Ed25519 over BCS, nonce-tracked, TTL-bounded). Cash-secured puts (`put_bucket`) mirror the covered-call design with cash collateral.

## On-chain contracts (Sui Move) — [`contracts/`](./contracts)

The protocol ships as Move packages with one-way boundaries. The primary package is **`core`** (`options_core`, zero third-party deps):

| Module | Responsibility |
|--------|----------------|
| [`bucket.move`](./contracts/core/sources/bucket.move) | `Bucket` shared object, cursor logic, write / exercise / redeem / cleanup |
| [`put_bucket.move`](./contracts/core/sources/put_bucket.move) | Cash-secured-put twin of `bucket.move` |
| [`position.move`](./contracts/core/sources/position.move) | `Position` object (write-range `[start, end)`) + redemption math |
| [`quote.move`](./contracts/core/sources/quote.move) | `Quote` struct + Ed25519 signature verification + nonce tracking |
| [`quote_signer.move`](./contracts/core/sources/quote_signer.move) | `QuoteSigner` (signing key + nonces; v0.3 collateral abstraction) |
| [`collateral.move`](./contracts/core/sources/collateral.move) | `CollateralRequest` hot-potato flow routing collateral release to external packages |
| [`admin.move`](./contracts/core/sources/admin.move) · [`treasury.move`](./contracts/core/sources/treasury.move) | `AdminCap`, protocol config, fee treasury |
| [`events.move`](./contracts/core/sources/events.move) · [`errors.move`](./contracts/core/sources/errors.move) | Event types and error codes |

Quote-driven collateral custody lives outside core (v0.3): the signed `Quote` carries `collateral_source` / `release_package` / `release_module` fields that route collateral release to any package implementing the standardized `release<T>` interface — the first-party implementation is [`contracts/mm-collateral`](./contracts/mm-collateral). Historical packages that no longer ship live under [`contracts/.deprecated/`](./contracts/.deprecated).

```
cd contracts/core && sui move test && sui move build
```

## Off-chain services (Rust) — [`rust-backend/`](./rust-backend)

The spec defines two off-chain deliverables, both under [`rust-backend/services/`](./rust-backend/services):

- **quoting-service** — a stateful WebSocket RFQ router between retail frontends and MM bots. Authenticates MMs by a signing-key challenge, broadcasts RFQs to the opposite side, validates returned signed quotes (signature, expiry, balance feasibility), and returns them to retail sorted by price. **Holds no funds and signs nothing** — it is a routing and bookkeeping layer; the on-chain revert is always the safety net.
- **indexer** — tails the chain's event stream for the protocol package, persists every event, materializes derived views (per-account balances, per-bucket cursor state, per-position status), and fans events out to the quoting service and frontends. Read-only with respect to the chain; kept separate so indexing load never touches quoting latency.

The canonical quote format shared by the services and the chain is the BCS encoding of the `Quote` struct, transmitted over WebSocket as JSON with numeric fields as decimal strings (see spec §4).

```
cd rust-backend && cargo check --workspace && cargo test --workspace
```

## Frontend — [`frontend/`](./frontend)

React dApp implementing the spec's retail flows: browse buckets (populated from the indexer, with live queue-position display from bucket cursor updates), request quotes over the RFQ WebSocket, execute the chosen signed quote in a PTB, and manage positions (exercise before expiry, redeem after). MM bots are not part of the deliverables — the spec defines only their interface.

```
cd frontend && npm install && npm run dev
```

## Repository map

```
options/
├── docs/options-protocol-spec.md   # The protocol design spec
├── contracts/
│   ├── core/                       # options_core — the protocol package
│   ├── mm-collateral/              # first-party collateral-release implementation
│   └── .deprecated/                # retired packages, kept for history
├── rust-backend/
│   └── services/
│       ├── quoting-service/        # WebSocket RFQ router (spec §5)
│       └── indexer/                # event indexer + fanout (spec §6)
├── frontend/                       # React dApp (spec §7)
└── .deprecated/                    # retired top-level projects
```
