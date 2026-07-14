# solana-api-service (`services/solana-api-service`)

Clone of `api-service`: **stateless** REST read model, every request a JIT
GraphQL query against **solana-indexer** via `crates/solana-indexer-graphql`.
Main-workspace member (no solana-sdk: base58 strings + borsh mirror decode).

## What stays identical

- Port 9003; axum; CORS; `/metrics`; boot-time token catalog from
  solana-token-info (hard cutover).
- Route surface and DTO philosophy (dual scaled-f64 + raw-string numerics,
  series grouping, FIFO PnL ledger, Black-Scholes `/options/metrics` via the
  `pricing` crate).
- Config: `bind_addr`, `indexer_graphql_url` (→ solana-indexer 9002),
  `allowed_origins`, `token_info_url` (→ solana-token-info),
  `derived_metrics_url` (→ solana-price-charting `/vault-apy`),
  `solana_rpc_url` (replaces `sui_rpc_url`), `price_charting_url` (optional —
  see PnL note). No secrets.

## What changes

- **Ids**: wallet/bucket/vault params are base58 pubkeys; `tx_hash` fields
  carry the transaction `signature`.
- **Catalog**: keyed by mint (byte-exact); no Move-type canonicalization.
- **`/rfqs` → `/auctions`**: the venue's generalized auctions replace
  RfqAuction/PutRfqAuction/SwapAuction.
  - `GET /auctions?status=<open|settled|unsold>&mode=<swap|covered_call|cash_secured_put>&bucket=<pk>&creator=<pk>`
    → `AuctionsResponse { auctions: [AuctionDto] }` where `AuctionDto` carries
    `auction_id, mode, bucket?, escrow_mint, bid_mint, amount_raw, notional_raw,
    reserve_bid_raw, best_bid_raw?, best_bidder?, deadline_ms, min_increment_bps,
    settle_authority?, status` (+ scaled floats using the catalog).
  - `GET /auctions/:auction_id/bids` → bid history from indexer `auctionBids`.
- **Buckets**: `tradeable` loses the DeepBook-pool condition (no order book
  yet) → `tradeable = !cleaned && !invalidated && !expired`. No
  `deepbook_pool_id` field. `option_kind` from the indexer (`call`/`put`).
- **Vault live read** (`solana_rpc.rs` replaces `sui_rpc.rs`): JSON-RPC
  `getAccountInfo` (base64) on the vault pubkey; strip the 8-byte Anchor
  discriminator (checked against the known value); borsh-decode a **mirror
  struct** of `options_vault::state::Vault` (solana-indexer `events.rs`
  pattern — no anchor dep). Best-effort like today: RPC failure omits live
  fields, never 5xxs. `phase` decodes cleanly (borsh enum — no Sui lossy-enum
  workaround needed).
- **PnL** (`/dashboard/pnl`): the DeepBook BalanceManager leg disappears
  (no order book). The FIFO ledger keeps write/exercise/redeem legs from
  events; the exercise *mark* falls back to `strike` when
  `price_charting_url` has no data (which, with no ingestion, is always for
  now). `bm` query param dropped.
- **Events feed** (`/events?wallet=`): branch on the Solana event families —
  `WriteExecuted`, `PutWriteExecuted`, `CollateralizedWrite`,
  `PutCollateralizedWrite`, `Exercised`/`PutExercised`,
  `Redeemed`/`PutRedeemed`, `AccountDeposit`, `AccountWithdraw`,
  `ExpiredOptionBurned`/`PutExpiredOptionBurned`. Auction events surface as
  `auction_bid` / `auction_settled` rows for the participant filter.
- **Vault DTOs**: `PPS_SCALE = 1e12` unchanged (options_math). Receipts are
  fresh-keypair accounts — ids are their pubkeys.

## crates/solana-indexer-graphql (built with this service; shared)

Modeled on `crates/indexer-graphql`, targeting the solana-indexer schema
(see `SOLANA_INDEXER_INTEGRATION_GUIDE.md`):

- DTOs: `Bucket` (mints, strikeRaw/strikeScale, expiryMs, totalWrittenRaw,
  exerciseCursorRaw, cleaned, invalidated, optionKind), `Account`, `Position`,
  `Auction`, `AuctionBid`, `Vault`, `VaultRound`, `VaultApyPoint`,
  `VaultReceipt`, `Progress { start_slot, current_slot, finalized_slot, … }`.
- Methods: `bucket`, `buckets`, `account`, `positions_by_recipient`,
  `positions_by_ids`, `auctions(status, mode, bucket_id, creator)`,
  `auction_bids`, `vaults`, `vault`, `vault_rounds`, `vault_apy`,
  `vault_receipts`, `head_sequence`, `events` scan (paginated, `sequenceGt`,
  optional `finalizedOnly`), `write_executed_for_account_since`,
  `write_executed_for_recipient` (+ put variants), `events_for_participant`,
  `progress`.
- Decimal-string parsing at the edge; `observability::client::instrumented`.

## Verification

- Unit: DTO mapping from canned GraphQL JSON fixtures; series grouping;
  FIFO ledger with auction-era events; borsh vault mirror decode against a
  fixture produced by the program crate (generate once with a small test in
  solana-contracts and commit the bytes).
- Integration: against a locally-running solana-indexer if available, else
  fixtures.
