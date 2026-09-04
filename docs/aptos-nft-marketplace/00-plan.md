# Aptos NFT marketplace: implementation plan

Status: **PLANNED, not scheduled.** One document: what exists on Aptos today,
what we build, in what order, and how each step is verified. The address
list lives in `appendix-addresses.md`. Facts about mainnet were read directly
from the chain on 2026-09-02 (module ABIs, package registries, hosted
indexer); anything not verified is marked **unverified**.

## 0. Decisions (agreed)

1. **Aggregator first, venue second.** Every live marketplace contract on
   Aptos exposes its buy path as `public entry`, so one router module of
   ours fills listings on any of them in one transaction. Tradeport's
   `markets_v2::buy_tokens_v2` did exactly this with one-function adapter
   modules per venue plus a Tradeport fee. Day-one liquidity is everything
   already listed on Aptos.
2. **Own venue = fork of the `aptos-core` reference marketplace**
   (`aptos-move/move-examples/marketplace`), which Wapal, Rarible and Topaz
   v2 also forked. Its payment leg is `Coin<CoinType>`-generic; we replace
   it with Fungible Asset (FA) so listings can be priced in APT, USDC or
   USDt, and the quote token becomes a data field, not a type parameter.
3. **Quote tokens are allowlisted.** Any FA the admin enables, seeded with
   APT (`0xa`), USDC, USDt. Native USDC and USDt have no Coin type at all,
   and both carry dispatchable transfer hooks, so a permissionless quote
   token is both a liveness and a griefing risk. Floor and volume also need
   a common denominator.
4. **Fees.** Percentage commission from the seller's proceeds on our-venue
   fills; a buyer-side add-on on routed buys through foreign venues; zero
   listing and bidding fees; per-token `min_fee`, per-collection overrides,
   per-wallet discounts. Fees accrue in the trade's quote token, no on-chain
   swap.
5. **Own the index.** The hosted NFT Aggregator GraphQL endpoint returned
   404 anonymously today (**unverified** whether it moved behind API keys).
   Marketplace state comes from our own Transaction Stream processor. The
   hosted Indexer GraphQL is used for token metadata and ownership at P0 and
   treated as a rebuildable cache, never as the source of truth for
   listings.
6. **Tradeport's contract is a live venue.** Its frontend shut down; the
   contract had 50 transactions in the last 1.3 days (27 new listings, 12
   unlists, 5 router buys). We index it, route buys to it, and ship a
   "rescue my escrowed NFTs" flow on day one.
7. **Repo layout.** `aptos/contracts` (Aptos Move, Aptos CLI toolchain, own
   CI) and `aptos/go-backend` (separate Go module; copies the four platform
   packages it needs from `go-backend/internal/platform`). New
   `aptos-frontend/` Vite app.
8. **Own DigitalOcean droplet, mainnet from day one.** The marketplace runs
   on a third droplet in `infra-do` (`s-2vcpu-4gb`, the same size as the two
   existing hosts), with Postgres in the compose stack and images on DO
   Spaces. There is no testnet or staging environment: contracts are
   unit-tested and exercised on a local testnet, then deployed to mainnet,
   and every gate below is a mainnet check. Marginal cost is about $29 a
   month (§7.1).

## 1. What is on chain (condensed)

Token standards, both mandatory: legacy TokenV1 (`0x3::token`, identity is
`TokenId { creator, collection_name, token_name, property_version }`, every
2022-2023 collection) and Digital Asset v2 (`0x4::token`, identity is the
object address, everything since late 2023). Every read model keys on a
canonical `token_data_id` that covers both, matching the Aptos indexer's
`current_token_ownerships_v2`. Royalties: `0x4::royalty` for v2, `TokenData`
for v1; the reference `listing::compute_royalty` handles both.

| Venue | Contract | Upgrade policy | Buy entry (all `public entry`) | Status 2026-09-02 |
|---|---|---|---|---|
| Tradeport | `0xe11c...3c26` | compatible, 20 upgrades | `listings_v2::buy_token(Object<Listing>)`, `listings::buy_token(creator, collection, name, pv)` for v1; router `markets_v2::buy_tokens_v2(...)` | contract busiest on chain; frontend gone |
| Wapal | `0x584b...58c9` | compatible, 15 | `coin_listing::purchase<T>(Object<Listing>)`, `purchase_many<T>` | live, ~2 tx/day |
| Rarible | `0x465a...e790` | **immutable** | `coin_listing::purchase<T>(Object<Listing>)`, auctions via `bid<T>` | live, volume **unverified** |
| Bluemove v2 | `0xd520...6f5` | compatible | `coin_listing::purchase<T>(Object<Listing>, price)` | activity **unverified** |
| Bluemove v1 | `0xd1fd...614e` | compatible | `marketplaceV2::batch_buy_script(creators, collections, names, pvs)` | dormant since 2025-07 |
| OKX | `0x1e60...7a43` | compatible | `okx_fixed_price::buy_direct_listing<T>(listing, price)` | dormant since 2025-07 |
| Topaz v1 | `0x2c7b...10a2` | compatible | `marketplace_v2::buy<T>(seller, id, price, creator, collection, name, pv)` | deprecated 2023; residual listings still bought through Tradeport's router |
| Topaz v2 | `0x6de3...67e0` | compatible | reference surface | dormant |
| Souffl3, Seashrine, Ozozoz, Mercato | see appendix | | | dead; history only |

Wapal, Rarible, Topaz v2 (and Bluemove v2 with tweaks) share the reference
surface: `coin_listing`, `fee_schedule`, `listing`, `token_offer`,
`collection_offer`, `events`, `marketplace_scripts`. One adapter covers them.

Data sources: Transaction Stream gRPC (`grpc.mainnet.aptoslabs.com:443`, API
key) is the primary feed; hosted Indexer GraphQL
(`api.mainnet.aptoslabs.com/v1/graphql`) for `current_token_datas_v2`,
`current_collections_v2`, `current_token_ownerships_v2`, ANS; fullnode REST
for ABIs, views, simulation, submission. The hosted indexer's `events` table
is deprecated (2026-09-08) and the public fullnode returns `410 Gone` beyond
its prune window, so all history comes from our own store. The open-source
`aptos-labs/aptos-nft-aggregator` (Rust) has the per-venue event mappings
we copy.

## 2. Architecture

```
                 Transaction Stream (gRPC)            hosted Indexer GraphQL
                          |                                    |
                 aptos/go-backend/cmd/nft-indexer  <-----------+  (metadata, ownership, ANS)
                          |  venue event mappers + our-venue mapper + router attribution
                          v
                      Postgres (nft_*)
                          |
                 aptos/go-backend/cmd/nft-api  --- REST + WS ---> aptos-frontend (Vite/React)
                          |  payload builders (tier 0 direct, tier 1 router), simulation,
                          |  image proxy/CDN, admin mux
                          v
              fullnode REST (simulate/submit)      wallet signs (wallet adapter, AIP-62)
                          |
      aptos/contracts/marketplace (our venue)   aptos/contracts/router (adapters -> foreign venues)
```

Two Go services, two Move packages, one frontend. Everything read-side is
rebuildable from the stream plus the hosted indexer.

## 3. Contracts (`aptos/contracts`)

Toolchain: Aptos CLI (pinned version in CI via the official
`install_cli.sh`), `aptos move test --dev`, compiler v2. Deployment as
**object code deployment** (`aptos move deploy-object`, upgrades with
`aptos move upgrade-object`), policy `compatible`, owner is a
`0x1::multisig_account` on mainnet and a plain key on testnet. Package ids
and object addresses recorded in `aptos/deployments.json` keyed by env,
mirroring `rust-backend/deployments.json`.

### 3.1 `marketplace` package (our venue)

Fork of the reference marketplace; keep its module boundaries and event
names so the indexer mapper for "ours" is the Wapal mapper with a different
address.

| Module | Keep / change |
|---|---|
| `listing` | Keep: `Listing` object holding the token (v2 via object transfer, v1 via `TokenV1Container`), `compute_royalty`, `close`. Change: add `quote: Object<Metadata>` field. |
| `fa_listing` (replaces `coin_listing`) | `init_fixed_price(seller, token: Object<ObjectCore>, quote, price)`, `init_fixed_price_for_tokenv1(...)`, `init_fixed_price_many`, `update_fixed_price`, `end_fixed_price`, `purchase(buyer, listing)`, `purchase_many`. Payment via `primary_fungible_store::withdraw(buyer, quote, price)`; royalty, commission, seller paid with `primary_fungible_store::deposit`. No type parameters. Auctions dropped at P0 (module stays out of the build; P2). |
| `token_offer`, `collection_offer` | Same FA change: offer escrow is an object-owned `FungibleStore` in the quote token; `sell_tokenv2`, `sell_tokenv1_entry`, `cancel`, expiry. Collection offers carry `remaining` quantity. |
| `fee_schedule` | Keep percentage commission, fee address, mutation events. Add `QuoteAllowlist { tokens: SmartTable<address, QuoteToken { enabled, min_fee }> }` under the same admin object, and per-collection commission overrides (`upsert_collection_numerator`, Tradeport's shape) and per-wallet discounts (`loyalty`). Zero listing and bidding fees (functions kept, set to 0, never surfaced). |
| `events` | Keep reference event structs (`ListingPlacedEvent`, `ListingFilledEvent`, ... with `TokenMetadata`), add `quote: address` and `fee: u64` fields. |
| `marketplace_scripts` | Vector-of-address convenience entries, as in reference. |

Invariants to test (Move unit tests, `--dev`):

- Fill pays exactly `price = royalty + commission + seller_proceeds`, in the
  listing's quote token, with `commission >= min_fee` for that token.
- Listing, offering, or filling in a non-allowlisted or disabled quote token
  aborts; disabling a token after a listing exists still allows `end_fixed_price`
  (seller can always get the NFT back) but blocks `purchase`.
- Both token standards round-trip (list, buy, cancel) with royalties honoured.
- Collection offer with quantity N fills N times then is closed; expired
  offers cannot be filled but can be cancelled by anyone (refund to bidder).
- Fee-schedule mutations apply to existing listings (listings reference the
  shared schedule object, not a snapshot).

### 3.2 `router` package (cross-venue)

```
router::buy_many(buyer, venues: vector<u8>, listings: vector<address>,
                 expected_prices: vector<u64>, extra: vector<u64>)
router::sell_to_offer(seller, venue: u8, offer: address, token: address)      // P1
```

Adapters, one function each, calling the venue's `public entry` purchase
(a `public entry` function is callable from another module):

| Adapter | Calls | Notes |
|---|---|---|
| `ref_market_adapter` | `wapal::coin_listing::purchase<AptosCoin>`, `rarible::coin_listing::purchase<AptosCoin>`, `topaz_v2::coin_listing::purchase<AptosCoin>` | Reads `coin_listing::price` view and asserts `== expected_price` before calling |
| `tradeport_adapter` | `listings_v2::buy_token(Object<Listing>)`; v1 `listings::buy_token(creator, collection, name, pv)` | v1 args packed by the payload builder |
| `bluemove_v2_adapter` | `coin_listing::purchase<AptosCoin>(listing, price)` | |
| `okx_adapter` | `okx_fixed_price::buy_direct_listing<AptosCoin>(listing, price)` | Dormant; include only if the spike shows live listings |
| `topaz_v1_adapter`, `bluemove_v1_adapter` | table-based `buy` / `batch_buy_script` | P1; Topaz signed listings (`buy_v2` with signature) are dead liquidity and are indexed as cancelled when simulation fails |

Fee: `buy_many` withdraws `sum(expected_prices) * fee_bps / 10_000`
(floored at `min_fee`) from the buyer in APT and deposits it to the treasury
before dispatching; emits `RouterFill { venue, listing, price, fee, buyer }`
per item. Sweeps are atomic at P0 (any stale listing aborts the whole
transaction; the API re-simulates right before signing). P1: best-effort on
object-based venues (skip when the `price` view mismatches or the object is
gone, emit `Skipped`).

Dependencies: foreign packages are fetched with `aptos move download` into
`router/deps/<venue>/` and referenced as local deps. Only Rarible and
Bluemove v1 publish source; the others are bytecode-only. Phase 0 spike
confirms the compiler links against bytecode packages (the TS SDK Script
Composer does this at runtime, so the linker path exists). Fallback: build a
Move script per transaction (same adapters as script code, fee as a
`coin::transfer` in the same script).

Foreign venue commission still goes to that venue; ours is additive on the
buyer side. `compatible` upgrade policy on every venue means their public
function signatures cannot change under us; Rarible is immutable.

### 3.3 Testing the router

Local testnet (`aptos node run-local-testnet`) with our own reference fork
published at Wapal's, Rarible's and Topaz v2's addresses via named-address
substitution (they are the same code shape), plus Bluemove v1 from its
published source. Adapter tests list on the "foreign" venue and buy through
`router::buy_many`. Mainnet acceptance: one canary buy per live venue of the
cheapest listing on a verified collection, from a dedicated ops wallet,
before the router address is enabled in the API.

## 4. Indexer (`aptos/go-backend/cmd/nft-indexer`)

Go 1.24, module `github.com/ewitulsk/SuiOptions/aptos/go-backend`, pgx/v5,
goose migrations embedded, TOML config with `${VAR}` expansion, Prometheus
`/metrics`, `/health` returning `ok`, `slog` structured logging (new for the
Go side; required so `alert_id` fields exist for Loki grouping).

### 4.1 Stream client

`internal/stream`: generated Go from the `aptos.indexer.v1` protos in
`aptos-core/protos` (vendored; `buf generate`), `RawData/GetTransactions`
with `starting_version`, `Authorization: Bearer <key>`. Processor loop:
batch of N transactions, apply all mappers, one DB transaction per batch,
commit the batch's last version to `stream_cursors(stream_name, version)`;
on crash replay from the cursor (upserts are idempotent on
`(txn_version, event_index, marketplace)`). Metrics: `nft_indexer_lag_seconds`,
`nft_indexer_batch_versions`, `nft_indexer_mapper_errors_total{venue}`.

### 4.2 Mappers

One package per venue under `internal/venues/<name>` implementing:

```go
type Mapper interface {
    Marketplace() string
    ContractAddress() string
    Map(tx *Transaction) ([]Activity, error)   // raw events -> normalized rows
}
```

Normalized rows and current-state tables use the Aptos NFT Aggregator
schema so its config file is a direct reference:

| Venue | listing created / cancelled / filled | token offer placed / cancelled / filled | collection offer placed / cancelled / filled |
|---|---|---|---|
| Tradeport v2 | `listings_v2::InsertListingEvent` / `DeleteListingEvent` / `BuyEvent` | `biddings_v2::InsertTokenBidEvent` / `DeleteTokenBidEvent` / `AcceptTokenBidEvent` | `biddings_v2::InsertCollectionBidEvent` / `DeleteCollectionBidEvent` / `AcceptCollectionBidEvent` |
| Tradeport v1 | `listings::InsertListingEvent` / `DeleteListingEvent` / `BuyEvent` (+`UpdateListingEvent`) | `biddings::Insert/Delete/AcceptTokenBidEvent` | `biddings::Insert/Delete/AcceptCollectionBidEvent` |
| Wapal, ours | `events::ListingPlacedEvent` / `ListingCanceledEvent` / `ListingFilledEvent` | `events::TokenOffer{Placed,Canceled,Filled}Event` | `events::CollectionOffer{Placed,Canceled,Filled}Event` |
| Rarible | `events::ListingPlaced` / `ListingCanceled` / `ListingFilled` | `events::TokenOffer{Placed,Canceled,Filled}` | `events::CollectionOffer{Placed,Canceled,Filled}` |
| Bluemove v1 | `marketplaceV2::ListEvent` / `DelistEvent` / `BuyEvent` | `offer_lib::OfferEvent` / `CancelOfferEvent` / `AcceptOfferEvent` | `offer_lib::OfferCollectionEvent` / `CancelOfferCollectionEvent` / `AcceptOfferCollectionEvent` |
| Topaz v1 | `events::ListEvent` / `DelistEvent` / `BuyEvent` | `events::BidEvent` / `CancelBidEvent` / `SellEvent` | `events::CollectionBidEvent` / `CancelCollectionBidEvent` / `FillCollectionBidEvent` |
| Router (ours) | `RouterFill` recorded as attribution only, never as a second sale | | |

`token_data_id` derivation: reference-style events carry
`TokenMetadata { creator_address, collection_name, token_name, token: Option<Object<Token>>, property_version: Option<u64> }`
so v1 or v2 is decided per event; Tradeport v1, Bluemove v1 and Topaz emit
`0x3::token::TokenId` and need the v1 hash. Addresses are canonicalized
(`0x`-prefixed, 64 hex) at the mapper boundary, the Aptos twin of the
`.claude/move-type-normalization.md` rule.

Fixtures: the Aptos docs list one example transaction version per event
type per venue (e.g. Wapal `listing_filled` at 2382221134, Tradeport v2
`listing_filled` at 2386455218, Rarible `listing_placed` at 2417694028).
Fetch each once from the stream, store as JSON under
`internal/venues/<name>/testdata/`, and assert the exact normalized row.
This is the mapper regression suite.

### 4.3 Metadata and ownership

P0: `internal/metadata` pulls `current_token_datas_v2`,
`current_collections_v2`, `current_token_ownerships_v2` from the hosted
indexer for every `token_data_id` / `collection_id` the mappers touch (lazy,
cached in `nft_tokens` / `nft_collections` / `nft_ownerships`, refreshed on
each sale and on demand). Attributes parsed from `token_properties`. Image
pipeline: `token_uri` fetched through IPFS/Arweave/HTTP gateways, stored in
S3, resized variants served from CloudFront; the API never returns a raw
`token_uri` to the browser. P1: own token processor from the stream
(write-set changes on `0x4::token::Token`, `0x4::collection::Collection`,
`0x3::token::TokenStore` table items) so the hosted indexer stops being a
dependency.

### 4.4 Backfill and pipeline progress

Every long-running fill of the read side records its progress in one table
so the status page (§6) and Prometheus read the same numbers:

```
pipeline_progress (pipeline, scope) PK
  pipeline   'stream' | 'metadata' | 'images' | 'stats'
  scope      venue name for 'stream' (one row per venue, plus 'all'); collection_id for others, plus 'all'
  target     tip version (stream) | tokens known (metadata) | images referenced (images)
  done       cursor version | tokens fetched | images stored
  failed     permanent failures (unreachable token_uri, unparsable metadata)
  started_at, updated_at, eta_at (linear estimate from the last 15 min of rate)
```

The stream row is derived from `stream_cursors` and the ledger tip; the
metadata and image rows are updated by their workers per batch. Every row is
also exported as `nft_pipeline_done{pipeline,scope}` / `nft_pipeline_target`
gauges. Failures keep a reason and are retried on a backoff schedule, with
a manual "retry" from the admin mux.

### 4.5 Tables

```
nft_marketplace_activities (txn_version, event_index, marketplace) PK
  listing_id, offer_id, raw_event_type, standard_event_type, creator_address,
  collection_id, collection_name, token_data_id, token_name, token_standard,
  price, quote_token, token_amount, buyer, seller, expiration_time,
  contract_address, block_timestamp, router_fee
current_nft_marketplace_listings (token_data_id, marketplace) PK
  listing_id, collection_id, seller, price, quote_token, token_amount,
  is_deleted, contract_address, last_transaction_version, last_transaction_timestamp
current_nft_marketplace_token_offers (token_data_id, buyer, marketplace) PK
current_nft_marketplace_collection_offers (collection_offer_id) PK  + remaining_token_amount
nft_collections, nft_tokens, nft_ownerships, nft_attributes, nft_images
collection_stats (collection_id, window) floor_apt, floor_usd, volume, sales, listed, owners
quote_tokens (metadata_address) symbol, decimals, enabled, min_fee, usd_price
admin_collections (collection_id) verified, hidden, featured, nsfw
stream_cursors (stream) version
pipeline_progress (pipeline, scope) target, done, failed, eta_at      (§4.4)
image_jobs (token_data_id) state, attempts, last_error, source_url, spaces_key
```

Floor and volume are computed per collection in a common denominator: APT
for the default UI, USD via `usd_price` refreshed from a DEX/oracle quote.
Listings in a token without a price feed count for the item page but not for
stats.

## 5. API (`aptos/go-backend/cmd/nft-api`)

Stdlib `net/http` with method+wildcard patterns, two muxes (public;
JWT-gated admin on a compose-internal port) as in `eventingestor/server`.
CORS from `platform/cors`.

Public:

```
GET  /collections?sort=volume_24h                 GET /collections/{id}            GET /collections/{id}/items?traits=&sort=price
GET  /collections/{id}/activity                   GET /items/{token_data_id}       GET /items/{token_data_id}/offers
GET  /wallets/{addr}/items                        GET /wallets/{addr}/escrowed     (Tradeport/Wapal/Topaz listings owned by addr)
GET  /wallets/{addr}/activity                     GET /search?q=
GET  /status                                       backfill and pipeline progress (§4.4), public, no auth
POST /tx/buy        {items:[{marketplace, listing_id}], buyer}     -> {payload, simulation, breakdown{price, venue_fee, our_fee, royalty}}
POST /tx/list       {token_data_id, quote, price}                  -> payload (our venue)
POST /tx/unlist     {marketplace, listing_id}                      -> payload (ours or foreign "rescue")
POST /tx/offer      {token_data_id | collection_id, quote, price, qty, expiry}
POST /tx/accept-offer / POST /tx/sell-to-offer                     (P1 for foreign)
POST /tx/submit     {signed_txn}  (optional; wallets may submit directly)
GET  /ws            listing_created | listing_filled | offer_* | stats per collection subscription
```

`GET /wallets/{addr}/items` returns every NFT the address holds in either
standard, **unioned with NFTs the address has escrowed in any venue's
listing object** (ours, Tradeport, Wapal, Topaz, Bluemove). This union
matters: the ownership table shows an escrowed NFT as owned by the listing
object, not the seller, so a naive holdings query hides exactly the NFTs a
Tradeport seller most wants to see. Each item carries `listed_on`,
`price`, `quote_token`, best offer, floor, and `rescuable: true` when the
listing is on a foreign venue. `addr` may be an ANS name.

Payload builders (`internal/venues/<name>/payload.go`): tier 0 direct entry
function to the foreign contract, tier 1 `router::buy_many` args. Every buy
payload is simulated against the fullnode before it is returned; a failed
simulation returns the reason (sold, delisted, price changed, insufficient
balance) and marks the listing stale for re-check. The API holds no keys and
signs nothing; the only server-side signer is the ops canary wallet, kept
out of the service.

Admin: verify / hide / feature / nsfw collections, quote-token list edits
(mirrors the on-chain allowlist; the on-chain change is a multisig
transaction), fee schedule display, re-index a collection, purge an image.

Alerting: per `.claude/tx-alerting.md`, `alert_id = "tx-failed-nft-api"`
on any submission failure at the handler (if `/tx/submit` is kept), and
`"tx-failed-nft-canary"` for the acceptance canary. Both added to that
file's list.

## 6. Frontend (`aptos-frontend/`)

Vite 5 + React 18 + TypeScript + react-router-dom 7 + TanStack Query 5, same
as `frontend/`. Wallets via `@aptos-labs/wallet-adapter-react` (Petra,
Pontem, Nightly, OKX, MSafe, Aptos Connect keyless login). Endpoints in
`src/config.ts` with `VITE_*` overrides and `127.0.0.1:90xx` defaults, like
`frontend/src/config.ts`. Vercel project with SPA rewrites.

Pages, P0:

- **Landing**: trending collections, recent sales.
- **Collection**: stats, item grid, attribute filters, activity.
- **Item**: buy from any venue, offers, price history.
- **Wallet** (`/wallet/{addr | ans}`, defaults to the connected wallet, any
  address viewable): every NFT the address owns in either standard,
  grouped by collection, with a badge for where each is listed and at what
  price, offers received, and total floor value. Items escrowed on Tradeport,
  Wapal, Topaz or Bluemove appear here too (see `GET /wallets/{addr}/items`
  in §5) with **Rescue** (unlist) and **Relist here** (unlist plus list in
  one transaction) actions. Bulk select for list / transfer / rescue.
- **Cart / sweep** with pre-flight breakdown (price, venue fee, our fee,
  royalty) and named failure reasons.
- **List / offer modals** with the quote-token picker.
- **Status** (`/status`, public): one card per pipeline from
  `GET /status`: stream indexer per venue (cursor vs tip, lag, backfill
  progress bar with ETA), metadata cache (tokens and collections fetched of
  known), images (fetched, resized, failed, with the failure reasons), and
  collection stats freshness. Shows the last deploy tag and the daily
  canary's last result. This is what we look at during the backfill and
  what users look at when a listing seems missing.
- **Admin** (behind JWT): verification, hiding, featuring, quote tokens,
  retry failed image and metadata jobs.

WS-driven live updates on collection, item and wallet pages.

## 7. Deployment: one droplet, mainnet only

### 7.1 Cost

DigitalOcean list prices (pricing page, 2026-09-02):

| Item | Plan | Monthly |
|---|---|---|
| Droplet | Basic `s-2vcpu-4gb`: 2 vCPU, 4 GiB, 80 GiB SSD, 4 TiB transfer | $24.00 |
| Spaces (images, metadata cache) | 250 GiB base subscription, $0.02/GiB beyond | $5.00 |
| Droplet backups (optional) | weekly, 20% of droplet price | $4.80 |
| **Marginal cost** | | **$29.00** (**$33.80** with backups) |

Smaller plans considered and rejected: `s-1vcpu-2gb` at $12 cannot hold
Postgres plus the gRPC stream decoder in RAM; `s-2vcpu-2gb` at $18 works
for the indexer alone but not once the image pipeline and API share the
host. Managed Postgres (from $15) is not needed at this volume; revisit if
the database outgrows the 80 GiB disk. Note `variables.tf` records that
nyc3 had no capacity above 4 GiB on 2026-09-01; the same constraint may
apply, and the droplet resizes in place if it does.

### 7.2 Host

Add a third droplet to `rust-backend/infra-do/main.tf`, copying the
`data_room` block: name `options-nft-host-do`, `var.nft_droplet_size`
defaulting to `s-2vcpu-4gb`, `ubuntu-22-04-x64`, same VPC, same SSH keys,
`user_data` from `deployment/do/host-bootstrap.sh` with `ROLE=nft`. Its
firewall opens 22, 80 and 443 to the world and 9100 (node-exporter) and the
service `/metrics` ports to the VPC range so the central Prometheus, Loki
and Grafana on `options-host-do` scrape and alert on it; nothing else. Add
`ROLE=nft` to the bootstrap script: nginx plus certbot for
`nft.<domain>` (edge nginx as in `deployment/do/edge-nginx.conf`, routing
`/api/*` and `/ws` to the API container and `/` to the static frontend
build), and `/opt/nft/{secrets,data}`. DNS: an A record for the droplet's
reserved IP. Add the droplet to `digitalocean_project_resources`.

Frontend hosting: the Vite build is served by the host nginx from the
droplet (one fewer moving part than a Vercel project; switch to Vercel later
if CDN latency matters).

### 7.3 Stack on the host

`aptos/deployment/docker-compose.yml`, one environment:

```
postgres:16        volume /opt/nft/data/pg, only on the compose network
nft-indexer        APTOS_STREAM_API_KEY, APTOS_INDEXER_API_KEY, DB_*; /metrics on 9041
nft-api            SPACES_*, JWT secret, DB_*; public 9040, admin 9042 (compose-internal only)
image-worker       part of nft-api at P0 (goroutine pool), own container later if needed
```

Secrets: `aptos/deployment/secrets.enc.yaml` (SOPS + age, decrypted on host
like the existing stacks). Postgres backups: nightly `pg_dump` to Spaces
from a cron container; the marketplace tables are rebuildable from the
stream, so the dump exists to skip a multi-hour backfill, not for
correctness.

### 7.4 Build and deploy

- `.github/workflows/aptos-go-ci.yml`: copy of `go-ci.yml` with
  `working-directory: aptos/go-backend`; gofmt gate, vet, tests against the
  `postgres:16` service. Paths `aptos/go-backend/**`.
- `.github/workflows/aptos-move-ci.yml`: matrix over `aptos/contracts/*`,
  pinned Aptos CLI, `aptos move test --dev`. Paths `aptos/contracts/**`.
- `.github/workflows/deploy-nft.yml`: on push to `staging` touching
  `aptos/**` (and on dispatch): build both images with `docker buildx bake -f
  aptos/deployment/bake.hcl --push` to the same registry the other services
  use, build the frontend, then SSH to `options-nft-host-do` with the
  existing `DEPLOY_SSH_KEY`, sync compose file and secrets, `docker compose
  pull && up -d`, copy the frontend build into the nginx root. The stack is
  independent of `rust-backend/deployment/affected.py`, `ec2/deploy.sh` and
  the Sui `bake.hcl`; nothing there changes.
- Contracts: `.github/workflows/deploy-nft-contracts.yml` (dispatch only)
  runs `aptos move deploy-object` / `upgrade-object` against **mainnet**
  with a deployer key from secrets, writes the object addresses into
  `aptos/deployments.json`, commits, and the API reads that file at boot.
  Ownership moves to a `0x1::multisig_account` once the contract is stable
  (§8, phase 3).
- Monitoring: the central Prometheus on `options-host-do` gets scrape
  targets for the new host over the VPC; gatus checks
  `https://nft.<domain>/api/health` for the literal `ok`; Grafana alert
  rows for `tx-failed-nft-canary` and for indexer lag above 120 s.
- `.claude/CLAUDE.md` project rules: add `aptos-addresses.md` (canonical
  64-hex `0x` form at every boundary; `token_data_id` derivation per
  standard) and extend `tx-alerting.md` with `tx-failed-nft-api` and
  `tx-failed-nft-canary`. Root `README.md` repository map gains an `aptos/`
  entry.

## 8. Phases and gates (mainnet throughout)

Durations assume two engineers (one Move, one Go/frontend) and overlap
where noted. Every gate is checked on mainnet against the production
droplet; the only pre-mainnet testing is Move unit tests and a local testnet
for the router adapters. Fees start at zero and are raised by a fee-schedule
mutation once the shakeout is over, so early mistakes cost nothing but gas.

**Phase 0: host and spikes (1 week).**
- Terraform the droplet (§7.2), bootstrap, DNS, TLS, empty compose stack up
  with Postgres, `deploy-nft.yml` green. Gate: `https://nft.<domain>/api/health`
  returns `ok` from a deployed image.
- Compile a one-function adapter against Wapal's downloaded bytecode
  package and fill a listing through it on a local testnet running our
  reference fork at Wapal's address. Gate: pass, or switch tier 1 to
  per-transaction Move scripts and pass the same check.
- Stream hello-world on the droplet: read 10k mainnet transactions, print
  every Wapal and Tradeport v2 marketplace event. Gate: the fixture versions
  in §4.2 decode to the expected event names.
- Hosted indexer API key and measured rate limit; aggregator endpoint status
  confirmed. Gate: written into this doc.

**Phase 1: indexer on mainnet (3 weeks, parallel with phase 2).**
- Stream client, cursor, batch loop, metrics, `slog`; mappers for Tradeport
  v1+v2, Wapal, Rarible, Bluemove v2 and OKX (drop any venue the 7-day
  stream shows has zero fills); fixture suite per venue.
- Metadata cache, image pipeline to Spaces, collection stats job,
  quote-token USD prices, `pipeline_progress` and `GET /status` (the
  status page itself is a plain HTML view of that endpoint until phase 5
  replaces it with the React page).
- Gate: backfill from the earliest Tradeport v2 transaction to tip on the
  droplet, watched on the status page; `current_nft_marketplace_listings` for three verified collections
  matches a hand-check on the explorer within one minute of tip; lag under
  30 s for 24 h; disk growth measured and extrapolated to stay under 80 GiB
  for a year.

**Phase 2: marketplace contract (3 weeks, parallel with phase 1).**
- Fork, FA payment leg, quote allowlist, fee schedule extensions, events;
  unit tests for the invariants in §3.1.
- Deploy to mainnet as an object under the deployer key; fee schedule
  initialised at 0 bps; allowlist seeded with APT, USDC, USDt.
- Gate: from two ops wallets on mainnet, the full matrix runs against the
  deployed contract with real but tiny amounts: list in APT and in USDC,
  buy, cancel, token offer, collection offer with quantity 2, accept, expire;
  royalties land on a creator address we control; our-venue mapper indexes
  every one of those fills correctly on the droplet.

**Phase 3: router (2 weeks, after phases 0 and 2).**
- Adapters for the kept venues, `buy_many`, fee, `RouterFill`; local-testnet
  suite; mainnet deployment.
- Gate: one canary buy per live venue on mainnet (cheapest listing on a
  verified collection, ops wallet) succeeds through the router with the
  expected fee split, and the indexer attributes it without a duplicate
  sale row. Then transfer package ownership to the multisig.

**Phase 4: API (2 weeks, overlaps phase 3).**
- Read endpoints, WS, payload builders (tier 0 first, then router),
  pre-flight simulation with named failure classes, escrowed-elsewhere
  endpoint, admin mux.
- Gate: a scripted end-to-end run against production lists, buys, offers
  and rescues with the ops wallets; simulation reports sold, delisted,
  repriced and insufficient-balance by name.

**Phase 5: frontend (4 weeks, starts with phase 4's read endpoints).**
- Pages in §6, wallet adapter, cart, quote-token picker, rescue flow;
  served from the droplet's nginx.
- Gate: a fresh wallet logs in with Aptos Connect, buys a Wapal-listed and
  a Tradeport-listed NFT in one sweep, lists one in USDC on our venue, and a
  second wallet buys it, all on mainnet; Lighthouse performance above 80 on
  collection pages with images from Spaces.

**Phase 6: open the doors (1 week).**
- Runbooks (indexer replay from a version, fee change via multisig, disable
  a quote token, hide a collection, restore Postgres from the nightly dump),
  daily canary buy scheduled, fee schedule raised to the launch rate,
  announce.
- Gate: 72 h with the indexer within 30 s of tip, zero `tx-failed-*`
  alerts, and a Tradeport-escrow rescue completed by an external user.

After launch, in order: instant-sell into foreign offers (router tier 2),
best-effort sweeps, rarity, price charts, public API and points, own token
processor, auctions, launchpad.

## 9. Risks

| Risk | Mitigation |
|---|---|
| Bytecode-dependency compile fails | Phase 0 gate; script fallback keeps the same adapters |
| A venue upgrades in a way that breaks an adapter | `compatible` policy forbids signature changes; the daily canary catches behavioural changes; adapter can be disabled per venue in the API without a redeploy |
| Hosted indexer rate limits or deprecations | Metadata is a cache; P1 own token processor removes the dependency |
| Malicious or hooked quote token | Allowlist; disabling a token always leaves `end_fixed_price` and offer cancel available |
| Stale-listing failures on sweeps | Simulation immediately before signing; best-effort sweeps at P1 |
| Spam collections and images | Admin hide/verify, heuristics, image proxy with size and type limits |
| Tradeport's frontend or wallets keep listing on their contract | Fine: we index and route to it; the rescue flow moves listings to us over time |
| No staging: a bad deploy hits production | Fees at zero during shakeout; every deploy is preceded by CI plus the fixture suite; compose rollback is `docker compose up` with the previous tag; the read side is rebuildable from the stream |
| Single droplet fails or fills its disk | Nightly `pg_dump` to Spaces; Terraform recreates the host in minutes; disk growth is a phase 1 gate |

## Revision history

- 2026-09-02: initial landscape, feature cut, and cross-venue design.
- 2026-09-02: added quote-token allowlist decision and fee placement.
- 2026-09-02: consolidated into a single implementation plan with phases and gates.
- 2026-09-02: own DigitalOcean droplet, mainnet-only delivery, cost estimate.
- 2026-09-02: status page for backfill and image pipeline; wallet page spelled out, escrowed-NFT union.
