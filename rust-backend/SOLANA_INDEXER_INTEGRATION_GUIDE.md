# Solana Indexer — Integration Guide

How to consume `services/solana-indexer` from another `rust-backend/`
service. The indexer ingests the three Solana programs' (`options_core`,
`auction_venue`, `options_vault`) `emit_cpi!` events from Helius
LaserStream and materialises them into Postgres; consumers read via a
GraphQL query API, never the tables directly.

It deliberately keeps the Sui indexer's surface conventions
(`crates/indexer-graphql` consumers will find everything familiar), with
Solana coordinates.

## Endpoints

| Surface | Docker network | Public (nginx) |
|---|---|---|
| GraphQL (`POST /graphql`) | `http://solana-indexer:9002/graphql` | `/{env}/solana-indexer/graphql` |
| Ingestion status (`GET /progress`) | `http://solana-indexer:9002/progress` | — (ops port serves health only) |
| Health / metrics | `http://solana-indexer:8081/{health,metrics}` | `/{env}/solana-indexer/health` |

GraphiQL playground + introspection are enabled on staging
(`expose_playground = true`), disabled on prod.

**Config convention:** add one key to your service's per-env TOML, mirroring
`indexer_graphql_url`:

```toml
solana_indexer_graphql_url = "http://solana-indexer:9002/graphql"   # staging/prod
# local dev: "http://127.0.0.1:9002/graphql"
```

## Wire conventions

- **Ids are base58 pubkeys** (`bucket_id`, `account_id`, `position_id`,
  `auction_id`, `vault_id`, mints, addresses). NOT `0x…` hex — a straight
  port of Sui-side id parsing will break.
- **All on-chain integers are decimal strings** (`*_raw` fields, `expiry_ms`,
  `sequence`, …) to dodge JS/JSON 53-bit loss. Parse to `u64`/`u128`.
- **Transactions are identified by `signature`** (base58), not tx digest.
- Event `payload` is the raw event JSON: field names match
  `solana-contracts/programs/*/src/events.rs` (snake_case), pubkeys base58,
  ints as strings, `Vec<u8>` as lowercase hex.
- The event log cursor is `sequence` (monotonic BIGSERIAL, assigned in
  chain order: slot → tx index → inner-instruction index).

## The two-tier reorg model — read this before wiring money logic

The indexer ingests at **`confirmed`** commitment (sub-second latency) and
tracks the **`finalized`** watermark (~15–30 s behind) in
`progress.finalized_slot`. Every event carries its `slot`.

- Rows with `slot <= finalized_slot` are immutable truth.
- Rows above it are *provisional*: a confirmed-level fork has never been
  observed on Solana mainnet, but if one ever lands the indexer evicts the
  forked slot's events and rebuilds its views (alert
  `solana-indexer-fork-evicted` fires).

Pick your tier per consumer:

| Consumer pattern | Tier | How |
|---|---|---|
| Quoting/UX reads (bucket state, auction status, balances for reservations) | confirmed | default queries; on-chain revert is the safety net |
| Anything additive you fold into YOUR OWN state (PnL, accounting, notifications you can't retract) | finalized | `events(finalizedOnly: true)` and only advance your cursor past sequences whose `slot <= finalizedSlot` |
| Vault PPS / round accounting displays | either | view tables are self-healing; label provisional data if you show it instantly |

The view tables (`buckets`, `vaults`, `auctions`, …) are confirmed-tier by
construction. If your service must never observe provisional state, derive
from `events(finalizedOnly: true)` instead of the views.

## Queries

Top-level queries (all JIT against Postgres):

- `bucket(id)`, `buckets(activeOnly, ids, underlyingMint, settlementMint, expiryMs, optionKind)`
- `account(id)` — MM account: owner, signing key, per-mint balances
- `positions(ids)`, `positionsByRecipient(recipient)` — enriched with bucket fields
- `auctions(status, mode, bucketId, creator)`, `auctionBids(auctionId)`
  — status: `open | settled | unsold`; mode: `swap | covered_call | cash_secured_put`
- `vaults`, `vault(id)`, `vaultRounds(vaultId)`, `vaultApy(vaultId)`,
  `vaultReceipts(vaultId, owner)`
- `events(filter, order, limit, after, finalizedOnly)` — the generalized log

### Event filter

Recursive input (`and` / `or` / `not`), everything at one level ANDed:

```graphql
{
  events(
    filter: {
      eventType: ["WriteExecuted", "CollateralizedWrite"]
      bucket: "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin"
      slotGte: 351234000
    }
    order: SEQUENCE_ASC
    limit: 200
    finalizedOnly: true
  ) {
    nodes { sequence slot signature eventType payload timestampMs }
    nextCursor
  }
}
```

- `participant: "<pubkey>"` — matches the address in ANY payload role
  (executor, recipient, bidder, …).
- `account` / `bucket` / `vault` / `auction` — sugar for
  `payloadContains: {"<field>": "<pubkey>"}` (GIN-indexed JSONB `@>`).
- `payloadContains: {"nonce": "42"}` — arbitrary containment; remember
  numeric payload values are strings.
- Cursor pagination: pass `nextCursor` back as `after`. Poll pattern
  (replaces the Sui WS fanout, same as the Sui indexer): remember the last
  `sequence` you processed, poll `events(filter: {sequenceGt: N}, order:
  SEQUENCE_ASC)`.

### Progress

`GET /progress` returns:

```json
{
  "start_slot": 351230000,
  "current_slot": 351234567,
  "finalized_slot": 351234530,
  "rate_slots_per_sec": 2.4,
  "ms_since_last_slot": 410
}
```

`ms_since_last_slot` beyond a few seconds means the stream is stalled
(Solana confirms ~2–3 slots/sec) — treat indexer data as stale, like the
Sui quoting-service's staleness threshold.

## Calling it from Rust

There is no `solana-indexer-graphql` client crate yet (build one modeled
on `crates/indexer-graphql` when a second consumer appears). For a first
consumer, inline reqwest is fine:

```rust
#[derive(serde::Deserialize)]
struct GqlResponse<T> { data: Option<T>, errors: Option<serde_json::Value> }

async fn bucket(client: &reqwest::Client, url: &str, id: &str) -> anyhow::Result<Option<Bucket>> {
    #[derive(serde::Deserialize)]
    struct Data { bucket: Option<Bucket> }
    let body = serde_json::json!({
        "query": "query($id:String!){ bucket(id:$id){ bucketId strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw cleaned invalidated optionKind } }",
        "variables": { "id": id },
    });
    let resp: GqlResponse<Data> = client.post(url).json(&body).send().await?
        .error_for_status()?.json().await?;
    if let Some(errs) = resp.errors {
        anyhow::bail!("graphql errors: {errs}");
    }
    Ok(resp.data.and_then(|d| d.bucket))
}
```

Deserialize `*_raw` strings with `str::parse::<u64>()` /
`::<u128>()` at the edge, exactly like `indexer-graphql` does.

## Porting a Sui `IndexerClient` consumer

| Sui indexer | solana-indexer |
|---|---|
| `checkpoint` | `slot` |
| `tx_digest` | `signature` |
| `event_index` | `inner_ix_index` (`txIndex` also available) |
| `0x…` object ids / addresses (hex) | base58 pubkeys |
| `asset_type` / `settlement_type` / `call_type` (Move type strings) | `underlying_mint` / `settlement_mint` / `option_mint` (mint pubkeys) — **no `to_canonical()` normalization needed**, base58 is byte-exact |
| `rfqs` / `rfqBids` | `auctions` / `auctionBids` (status `expired_unsold` → `unsold`; new `mode` field; pure swaps have `bucketId: null`) |
| `payloadContains: {"bucket_id": …}` | `payloadContains: {"bucket": …}` (field names follow the Solana events) |
| `head_sequence()` / WS fanout | same poll-by-`sequence` pattern via `events` |
| no reorg concept (checkpoints final) | `finalizedOnly` / `finalized_slot` — see the two-tier section |

Event tag names are unchanged where the concept survived
(`WriteExecuted`, `Exercised`, `VaultRoundFinalized`, …); the venue
family is new (`AuctionCreated`/`AuctionBid`/`AuctionSettled`/
`AuctionUnsold` replace `RfqCreated`/`RfqBid`/`RfqSettled`/
`RfqExpiredUnsold`, discriminated by `mode` instead of a `Put` prefix or
`Swap` name — vault-side echo events `VaultRfq*`/`VaultSwap*` still exist
in the log for vault-scoped filtering).

## Local development

1. Postgres (same local instance the Sui indexer uses, port 7654):
   `createdb -h localhost -p 7654 -U postgres solana_indexer`
2. Secrets file (anywhere, e.g. `services/solana-indexer/config/secrets.toml`
   — gitignored): `[helius]\napi_key = "<your key>"`
3. Run: `cd services/solana-indexer && cargo run -- --config config/config.toml --secrets config/secrets.toml`
   (standalone cargo workspace — build from the service dir, not the
   rust-backend root).
4. GraphiQL: http://127.0.0.1:9002/graphql — the schema is self-documenting.

Migrations are embedded and run at boot. The devnet program ids live in
the config TOMLs; on a fresh DB the indexer tails from the stream tip
unless `start_slot` pins a slot (must be within LaserStream's ~24h replay
window).

## Ops notes

- DB: `solana_indexer_<env>` on the shared RDS, provisioned via the
  wipe-provision-db workflow. First deploy of the service must be
  `force_all` (tag seeding).
- Secret: `options/<env>/solana-indexer` (`helius_api_key`); unfilled →
  the service crash-loops, other deploys unaffected.
- Alerts (generic tagged-error rule picks these up): `solana-indexer-stream-stalled`,
  `solana-indexer-stream-error`, `solana-indexer-decode-failed` (event
  schema drift vs the deployed programs), `solana-indexer-fork-evicted`,
  `solana-indexer-ingestion-died`.
- Metrics: `solana_indexer_slot_height`, `solana_indexer_finalized_slot`
  (should trail by ~32 slots), `solana_indexer_events_decoded_total`,
  `solana_indexer_slot_apply_duration_seconds`, DB pool/query gauges.
- Event mirror drift: `tests/idl_drift.rs` cross-checks against the
  committed IDL snapshots in `tests/fixtures/`. When the programs' events
  change: `anchor build`, copy the new IDLs over the fixtures, update
  `src/events.rs` — the test fails until they agree.
