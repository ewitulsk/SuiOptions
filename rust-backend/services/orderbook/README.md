# orderbook — hybrid exchange off-chain service

Off-chain half of the hybrid exchange (`contracts/exchange`): REST/WS
gateway + order intake, per-market price-time matching, matched-mode
settlement, and chain sync. The orderbook is trusted for liveness and
fairness of matching only — it cannot forge fills, misprice, or move funds
(every fill needs the maker's signature over exact terms; escrow debits
happen only inside the Move package).

## Structure

- Shared crates: `exchange-types` (Order BCS mirror, markets, u64 math),
  `exchange-signing` (digest + ed25519/secp256k1 verify + conformance
  fixtures — consensus-critical), `exchange-book` (matching engine),
  `exchange-router` (split-route planner).
- Service-local: `src/db/` (diesel/Postgres, indexer-style layout, embedded
  migrations), `src/sync.rs` (GraphQL event ingestion via
  `sui_tx::EventClient`, per-module cursors), `src/settlement.rs`
  (`match_orders` PTBs via `sui-tx` gRPC + Move-abort decoding into
  prune/restore/rematch), `src/intake.rs` (§5.4 pipeline), handlers/ws.

## Configuration

`config/config.{staging,prod}.toml` (+ `${VAR}` env expansion). Markets and
the exchange package id are read from `deployments.json`'s `exchange` block
— never from hand-maintained config, because market registry IDs are the
order-signature domain. The `exchange_markets` table is the serving
whitelist: boot mirrors the deployments set into it (new rows enabled,
rows absent from the record disabled — never deleted), and only enabled
rows are served/matched. Delisting a market without a redeploy is
`UPDATE exchange_markets SET enabled = false WHERE registry_id = …` plus a
restart; the boot upsert never touches the flag. The rendered `/run/secrets/orderbook.toml` carries
the relayer key and Sui endpoint overrides; both are optional — without a
key the service runs open-orderbook mode (serves signed fill tickets, no
matched settlement).

## API (spec §5.3)

- `GET  /v1/markets`, `GET /v1/markets/{m}/book?depth=N`, `GET /v1/markets/{m}/trades`
- `GET  /v1/markets/{m}/orders/{digest}` — the signed order IS the fill ticket
- `POST /v1/orders` — signed-order intake (signature, tick/size, expiry,
  salt monotonicity vs on-chain watermark, delegated-signer ACL, escrow
  coverage; write-ahead persist then match)
- `DELETE /v1/orders/{digest}` — soft cancel (signed payload; response
  states the on-chain-fillability caveat)
- `GET  /v1/accounts/{addr}/orders|fills|balance`
- `GET  /v1/routes?from=&to=&amount=` — split-route quote + PTB skeleton
- `WS   /v1/ws` — `book.{market}`, `trades.{market}`, `orders.{addr}`

## Operational notes

- FillEvents from chain sync are the single source of truth (external
  open-orderbook fills included); submitter confirmations are provisional.
- Escrow decreases / salt watermarks / signer removals prune resting orders
  event-driven (§5.7); failed settlements restore-and-rematch (§5.6).
- Alerting: `tx-failed-exchange-match-settlement` (submission failed after
  decode — not a benign race loss) and `tx-failed-exchange-match-queue`
  (dropped intent) per the tx-alerting rule.
- DB roles `orderbook_{staging,prod}` must exist before first deploy;
  migrations are embedded and run at boot.
