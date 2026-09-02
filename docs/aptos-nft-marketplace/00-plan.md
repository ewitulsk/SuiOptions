# Aptos NFT marketplace: landscape, feature set, and cross-market buy design

Status: **IDEATION**. This chapter answers three questions before any build
planning starts: what is live on Aptos today, what a marketplace must ship to
be credible, and how we let our users buy NFTs that are listed on *other*
marketplaces' contracts. Chapter `01-` (build plan) follows once the feature
cut is agreed.

Everything on-chain below was read directly from mainnet on 2026-09-02
(module ABIs via the fullnode REST API, package registries, and the hosted
indexer's `account_transactions` table). Where a claim could not be verified
it is marked **unverified**.

## Executive recommendation

1. **Build the aggregator first, the venue second.** Every live Aptos
   marketplace contract exposes its buy path as `public entry`, so a single
   router module of ours can fill listings on any of them in one transaction.
   Tradeport proved this exact design: its `markets_v2::buy_tokens_v2` is a
   router with adapter modules (`wapal_listings`, `bluemove_v2_listings`,
   `topaz_v2_listings`) that wrap the other venues' purchase functions and
   skim a Tradeport fee on top. Day-one liquidity is "everything already
   listed on Aptos", not "whatever our users list with us".
2. **Fork the `aptos-core` reference marketplace for our own venue.** Wapal,
   Rarible, and the Topaz v2 contract are all forks of
   `aptos-move/move-examples/marketplace` (same `coin_listing` /
   `fee_schedule` / `collection_offer` / `token_offer` module set, same event
   shapes). Starting from the same base gives us a reviewed design that
   already handles legacy TokenV1 and Digital Asset (v2) tokens, Coin and
   Fungible Asset payment, fixed price, auctions, token offers, and
   collection offers. It also means one adapter shape covers three foreign
   venues.
3. **Own the index.** Aptos Labs' hosted NFT Aggregator GraphQL endpoint
   returned 404 anonymously today (**unverified** whether it moved behind
   API keys or was retired). Do not build a product on it. Run our own
   processor over the Transaction Stream (gRPC) into Postgres, following the
   `go-backend/internal/eventingestor` poller idiom, and treat the aggregator
   only as an optional backfill source.
4. **Treat Tradeport's contract as a live venue, not a dead one.** Its
   frontend is what shut down; the contract is still the most active
   marketplace contract on the chain (see §2.3), because wallets and other
   frontends embed its SDK, and sellers are actively unlisting escrowed
   NFTs. Two product consequences: index and route buys for Tradeport
   listings, and ship a "rescue my listed NFTs" flow that calls
   `listings_v2::unlist_tokens` on the seller's behalf.
5. **Layout in this repo:** `aptos/contracts` (Aptos Move packages, Aptos
   CLI toolchain, own CI workflow mirroring `move-ci.yml`) and
   `aptos/go-backend` (a second Go module: indexer + API + router-payload
   builder, reusing the platform patterns from `go-backend/internal/platform`).

## 1. Token standards we must support

| Standard | Address | Identity of an NFT | Share of live listings |
|---|---|---|---|
| Legacy Token (TokenV1) | `0x3::token` | `TokenId { creator, collection_name, token_name, property_version }` | Large: every 2022-2023 collection (Aptos Monkeys, Aptomingos, ...). Every venue keeps a separate `*_tokenv1` code path. |
| Digital Asset (TokenV2 / DA) | `0x4::token`, `0x4::collection`, `0x4::royalty` | `Object<Token>` address | All collections minted since late 2023. |

Rules that fall out of this:

- Every read model keys NFTs by a canonical `token_data_id` that works for
  both standards (v1: hash of creator+collection+name; v2: object address).
  This is the same shape the Aptos indexer's `current_token_ownerships_v2`
  uses, so joins stay cheap.
- Every write path (list, buy, offer) has a v1 and a v2 variant. The
  reference marketplace already splits them (`init_fixed_price` vs
  `init_fixed_price_for_tokenv1`).
- Royalties are read from `0x4::royalty` for DA and from `TokenData` for
  v1; the reference `listing::compute_royalty` handles both and pays the
  creator on fill. Royalties are enforced by *our* contract on *our*
  listings; on foreign listings the foreign contract decides.
- Payment: APT as `Coin<AptosCoin>` today, plus Fungible Asset (FA) for
  stablecoins. The reference marketplace already accepts either.

## 2. The landscape (mainnet, 2026-09-02)

### 2.1 Venues and their contracts

| Venue | Contract account | Upgrade policy | Source on chain | Token support | Status |
|---|---|---|---|---|---|
| **Tradeport** | `0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26` | compatible (upgraded 20 times) | no | v1 (`listings`, `biddings`, `markets`) and v2 (`listings_v2`, `biddings_v2`, `markets_v2`) | Frontend shut down; contract live and busiest on chain |
| **Wapal** (Mokshya) | `0x584b50b999c78ade62f8359c91b5165ff390338d45f8e55969a04e65d76258c9` | compatible (15 upgrades) | no | v1 + v2 | Live, low volume |
| **Rarible** | `0x465a0051e8535859d4794f0af24dbf35c5349bedadab26404b20b825035ee790` | **immutable** | yes | v1 + v2 | Frontend live (**unverified** volume) |
| **Bluemove v1** | `0xd1fd99c1944b84d1670a2536417e997864ad12303d19eac725891691b04d614e` | compatible | yes | v1 only (`0x3::token::TokenId`) | No user activity since 2025-07 |
| **Bluemove v2** | `0xd520d8669b0a3de23119898dcdff3e0a27910db247663646ad18cf16e44c6f5` | compatible | no | v2 (reference-marketplace fork) | No indexed activity |
| **OKX NFT** | `0x1e6009ce9d288f3d5031c06ca0b19a334214ead798a0cb38808485bd6d997a43` | compatible | no | `okx_fixed_price::buy_direct_listing<T>` | No user activity since 2025-07 |
| Topaz (v1 contract) | `0x2c7bccf7b31baf770fdbcc768d9e9cb3d87805e255355df5db32ac9a669010a2` | compatible | no | v1 | Deprecated 2023; residual listings still being bought via Tradeport's router |
| Topaz v2 | `0x6de37368e31dff4580b211295198159ee6f98b42ffa93c5683bb955ca1be67e0` | compatible | no | reference-marketplace fork | Dormant |
| Souffl3, Seashrine, Ozozoz, Mercato | `0xf699...`, `0xd543...`, `0xded0...`, n/a | | | | Dead; only worth indexing for history |

The full address list, including Tradeport's own dependency graph (which is
the most complete "who existed" list on Aptos), is in
`docs/aptos-nft-marketplace/appendix-addresses.md`.

### 2.2 Buy entry points, per venue (verbatim from on-chain ABIs)

All of these are `public entry`, so they can be called both directly from a
transaction and from another Move module. `T` is the payment coin type
(`0x1::aptos_coin::AptosCoin` in practice).

Tradeport (native listings):

```
listings_v2::buy_token(&signer, Object<listings_v2::Listing>)             // DA tokens
listings_v2::buy_tokens(&signer, vector<Object<listings_v2::Listing>>)
listings::buy_token(&signer, creator: address, collection: String, name: String, property_version: u64)  // TokenV1
listings::buy_tokens(&signer, vector<address>, vector<String>, vector<String>, vector<u64>)
listings_v2::unlist_token(s)(...)   // the "rescue" path for sellers
```

Tradeport (its own cross-venue router, the design we copy):

```
markets_v2::buy_tokens_v2(&signer,
  vector<u64>      listing_ids,
  vector<String>   marketplace_names,      // "tradeport" | "wapal" | "topaz" | "bluemove" | ...
  vector<address>  creators,
  vector<u64>      prices,
  vector<address>  sellers,
  vector<String>   collection_names,
  vector<String>   token_names,
  vector<u64>      property_versions,
  vector<u128>     nonces,
  vector<address>  listing_objects)
wapal_listings::buy_token(&signer, listing: address)
bluemove_v2_listings::buy_token(&signer, listing: address, price: u64)
topaz_v2_listings::buy_token(&signer, listing: address)
```

Wapal, Rarible, Topaz v2 (identical reference-marketplace surface, different
account address):

```
coin_listing::purchase<T>(&signer, Object<listing::Listing>)
coin_listing::purchase_many<T>(&signer, vector<Object<listing::Listing>>)   // Wapal only
coin_listing::bid<T>(&signer, Object<Listing>, amount)                     // auctions (Rarible)
collection_offer::sell_tokenv2<T>(&signer, Object<CollectionOffer>, Object<0x4::token::Token>)
collection_offer::sell_tokenv1_entry<T>(&signer, Object<CollectionOffer>, token_name, property_version)
token_offer::sell_tokenv2<T>(&signer, Object<TokenOffer>)
token_offer::sell_tokenv1_entry<T>(&signer, Object<TokenOffer>, token_name, property_version)
```

Bluemove v2 (reference fork with tweaks):

```
coin_listing::purchase<T>(&signer, Object<listing::Listing>, price: u64)
coin_listing::batch_buy_token_v2<T0, T1>(&signer, vector<Object<T0>>, vector<u64>)
token_offer::sell_instantly_token_v2<T>(&signer, Object<TokenOffer>, Option<Object<Listing>>)
```

Bluemove v1 (TokenV1 only):

```
marketplaceV2::batch_buy_script(&signer, vector<address> creators, vector<String> collections, vector<String> names, vector<u64> property_versions)
marketplaceV2::accept_offer(&signer, offer_id, creator, collection, name, property_version)
```

OKX:

```
okx_fixed_price::buy_direct_listing<T>(&signer, listing: address, price: u64)
```

Topaz v1 (residual listings):

```
marketplace_v2::buy<T>(&signer, seller, listing_id, price, creator, collection, name, property_version)
marketplace_v2::buy_many_v2<T>(... vectors ..., vector<vector<u8>> signatures)   // Topaz-signed listings
collection_marketplace::fill<T>(&signer, bid_id, price, creator, collection, name, property_version)
```

### 2.3 Where the activity is right now

Last 50 user transactions touching each contract account, read from the
hosted indexer on 2026-09-02:

| Contract | Latest tx | Span of last 50 txs | What those txs were |
|---|---|---|---|
| Tradeport | 2026-09-02 12:12 UTC | **1.3 days** | 27 `markets_v2::list_tokens_v2`, 12 `listings_v2::unlist_tokens`, 5 `markets_v2::buy_tokens_v2`, 3 `biddings_v2::collection_bids` |
| Wapal | 2026-09-01 | 20.8 days | 24 buys, all via Tradeport's `markets_v2::buy_tokens_v2`; 13 collection-offer cancels; 8 delists |
| Topaz v1 | 2026-08-26 | 96.5 days | 30 buys via Tradeport's router, 9 lists, 7 collection-offer fills |
| Bluemove v1 | 2025-07-31 | 1016 days | package publishes and coin transfers only |
| OKX | 2025-07-31 | 1001 days | housekeeping only |
| Topaz v2 | 2025-07-30 | 741 days | housekeeping only |
| Rarible | (publish tx only) | n/a | Rarible stores listings as objects at other addresses, so this table is blind to it. **Unverified.** |
| Bluemove v2 | none | n/a | Same caveat as Rarible. **Unverified.** |

Reading: the Aptos NFT market today is small (tens of fills a day chain-wide)
and it flows almost entirely through Tradeport's router contract, whether the
listing sits on Tradeport, Wapal, or a 2023 Topaz escrow. Whoever indexes
those listings and offers a working buy button captures the whole market.
The 12 `unlist_tokens` in a day and a half are sellers pulling NFTs out of
Tradeport escrow by hand, which is the onboarding wedge in §3.6.

Two caveats. First, we cannot see from this table who is submitting the
`list_tokens_v2` calls (Petra and other wallets embed Tradeport's SDK, and
Tradeport's own site may still serve Aptos in read-only mode); it does not
change the design. Second, the hosted indexer deprecated its `events` table
on 2026-09-08 and the public fullnode returns `410 Gone` for transactions
older than its prune window, so all historical work needs our own store.

### 2.4 Data sources

| Source | What it gives | Access | Verdict |
|---|---|---|---|
| Transaction Stream Service (gRPC, `grpc.mainnet.aptoslabs.com:443`) | Every transaction with events and write sets, from any version | API key from Aptos Build (free tier) | **Primary feed** for our indexer |
| Hosted Indexer GraphQL (`api.mainnet.aptoslabs.com/v1/graphql`) | `current_token_ownerships_v2`, `current_token_datas_v2`, `current_collections_v2`, `account_transactions`, ANS names | Anonymous with low rate limits; API key for more | **Metadata + ownership backfill**, wallet portfolio reads |
| Aptos NFT Aggregator GraphQL (`.../nft-aggregator/v1/graphql`) | Normalized `current_nft_marketplace_listings` / `_token_offers` / `_collection_offers` / `nft_marketplace_activities` across Tradeport, Wapal, Bluemove, Rarible, Topaz | Returned **404** anonymously today | Nice-to-have backfill if it still exists behind a key; never a dependency |
| `aptos-labs/aptos-nft-aggregator` (Rust, open source) | The processor behind the table above: YAML config maps each venue's events to standard `listing_created` / `listing_filled` / `token_offer_*` / `collection_offer_*` rows | Self-host | **Fork its event mappings** into our Go processor; they are the exact schema we want |
| Fullnode REST (`fullnode.mainnet.aptoslabs.com/v1`) | Module ABIs, resources, view functions, simulation, submission | Anonymous | Tx build/simulate/submit, ABI-driven adapters |
| Tradeport / indexer.xyz GraphQL and `@tradeport/aptos-trading-sdk` | Their normalized listings API and payload builder | API key; company shut down | Do not depend on it. Useful only as a reference for payload shapes |

## 3. Feature set

Grouped by what a user is doing. "P0" is the launch cut; "P1" is the first
follow-up; "P2" is later. The cut is deliberately weighted toward buying
across venues, because that is where the liquidity is.

### 3.1 Discover

| Feature | Priority | Notes |
|---|---|---|
| Collection pages: floor, listed count, 24h/7d volume, sales, owners, supply | P0 | Floor and volume computed **across all venues**, not only ours |
| Item grid with attribute filters, price/rarity sort, "buy now only" | P0 | Attributes from `token_properties` (v1) / property map (v2) |
| Rarity ranks | P1 | Statistical rarity over the collection's attribute frequencies |
| Global search: collections, items, wallets, ANS names | P0 | ANS resolution via the indexer |
| Activity feed per collection / item / wallet | P0 | From `nft_marketplace_activities` |
| Price history chart per item and floor chart per collection | P1 | Reuse `lightweight-charts` already in `frontend/` |
| Trending / top collections landing | P0 | Volume-ranked over 24h / 7d |
| Collection verification badge, spam/scam hiding, NSFW flag | P0 | Admin-curated allowlist plus heuristics; spam is a real problem on Aptos |

### 3.2 Buy

| Feature | Priority | Notes |
|---|---|---|
| Buy a listing on **any venue** in one click | P0 | §4 |
| Sweep / cart: buy N cheapest across venues in one transaction | P0 | Router batch call; partial-fill semantics in §4.4 |
| Pre-flight simulation with clear failure reasons (sold, delisted, price changed) | P0 | Fullnode `/transactions/simulate`; stale listings are the number-one UX failure on aggregators |
| Fee and royalty breakdown before signing | P0 | Ours, the venue's, the creator's |
| Instant sell into best collection offer (any venue) | P1 | `collection_offer::sell_tokenv2` on foreign venues |
| Sponsored gas for first purchase | P2 | Aptos fee-payer transactions; there is a Sui gas station service in `rust-backend/` to crib from |

### 3.3 Sell

| Feature | Priority | Notes |
|---|---|---|
| List fixed price (single and bulk), edit price, delist | P0 | Our contract, both token standards |
| Offers: token offer, collection offer (with quantity), accept / cancel, expiry | P0 | Reference marketplace has all three |
| Auctions (timed, reserve, buy-now) | P2 | Reference marketplace has it; Wapal and Rarible ship it; volume is negligible |
| "Rescue" escrowed NFTs: unlist from Tradeport / Wapal / Topaz from our UI | P0 | §3.6 |
| Bulk transfer | P1 | |

### 3.4 Portfolio

| Feature | Priority | Notes |
|---|---|---|
| Holdings with floor value, listed-where badge, offers received | P0 | From `current_token_ownerships_v2` plus our listings/offers tables |
| Activity and realized P&L per wallet | P1 | |
| Hidden items | P1 | |
| Watchlist and price alerts | P2 | |

### 3.5 Creators

| Feature | Priority | Notes |
|---|---|---|
| Royalties honoured on our venue for both standards | P0 | Enforced in contract at fill |
| Creator dashboard (volume, royalties earned, holders) | P1 | |
| Launchpad / minting | P2 | Wapal's core business; not ours at launch |

### 3.6 Onboarding wedge: the Tradeport diaspora

Tradeport-listed NFTs are still sitting in Tradeport `Listing` objects. Their
owners have two options today: sign an `unlist` call by hand through an
explorer, or wait. We ship, on day one:

- A wallet view that shows every NFT the connected wallet has escrowed on
  Tradeport, Wapal, Topaz, and Bluemove, with a one-click unlist (or
  "relist with us at the same price" which composes unlist plus list in one
  transaction).
- Their Tradeport listings and collection bids kept live and buyable on our
  site until they choose to move them.

This is cheap (adapters we build anyway, §4) and it is the honest story for
launch: "your listings still work, and here is the button Tradeport no
longer gives you".

### 3.7 Platform

| Feature | Priority | Notes |
|---|---|---|
| Wallet adapter: Petra, Pontem, Nightly, OKX, MSafe, Aptos Connect (keyless Google/Apple login) | P0 | `@aptos-labs/wallet-adapter-react`, AIP-62 |
| Real-time updates (new listing, sold, offer) over WebSocket | P0 | Same fan-out idiom as the Rust indexer |
| Image/metadata pipeline: fetch IPFS/Arweave/HTTP `token_uri`, cache, resize, serve from CDN | P0 | The single biggest operational cost of an NFT marketplace; never hotlink |
| Public REST API (listings, offers, collections, activity) | P1 | Tradeport's developer API was a moat; wallets need somewhere to point |
| Points / loyalty, referral | P1 | Tradeport had per-wallet fee discounts (`loyalty` module); we already run a leaderboard service in `go-backend` |
| Admin: verify/hide collections, feature collections, fee schedule edits | P0 | JWT-gated admin mux like `eventingestor/api_admin` |
| Observability: Prometheus metrics, `/health`, `alert_id` on every tx-submission failure | P0 | `.claude/tx-alerting.md`; note Go has no structured logger yet |

## 4. Cross-venue buying: technical design

### 4.1 Why it works on Aptos

Three properties of Move on Aptos make aggregation straightforward:

1. Every venue's buy function is `public entry` (verified per function in
   §2.2). A `public entry` function can be called by another module, so our
   router can wrap them.
2. Packages with the `compatible` upgrade policy cannot remove or change
   the signature of a public function, so a compile-time dependency on
   Wapal or Tradeport does not break when they upgrade. Rarible is
   `immutable`, which is even safer.
3. The compiler can link against on-chain bytecode. Only Rarible and
   Bluemove v1 publish source, but `aptos move download` fetches the
   bytecode package and the Move compiler (and the TS SDK's Script
   Composer, which does this at runtime) links against it. This is the one
   item to spike before committing to the router (§5).

### 4.2 Three integration tiers

**Tier 0: direct payload.** Our backend builds the entry-function payload
against the foreign contract (for example
`0x584b...::coin_listing::purchase<AptosCoin>(listing_object)`), the user
signs it in their wallet, we submit or the wallet submits. No contract of
ours involved. Works today for every venue in §2.2, one listing per
transaction, no fee for us. This is the fallback and the first thing to ship
so buying works while the router is being written.

**Tier 1: router module (the Tradeport design).** A package
`aptos/contracts/router` with one adapter module per foreign venue and a
`router::buy_many` entry that takes parallel vectors of
`(venue_id, listing_address, expected_price, ...)`, dispatches to the right
adapter, charges our aggregator fee (basis points on `expected_price`, paid
to our treasury), and emits our own `RouterFill` event so our indexer
attributes volume. Adapter shape, for the three reference-marketplace forks:

```move
module router::ref_market_adapter {
    // Wapal, Rarible and Topaz v2 all expose this exact surface.
    public fun buy<CoinType>(buyer: &signer, listing: address, venue: u8) {
        if (venue == VENUE_WAPAL) {
            wapal::coin_listing::purchase<CoinType>(buyer, object::address_to_object(listing))
        } else if (venue == VENUE_RARIBLE) {
            rarible::coin_listing::purchase<CoinType>(buyer, object::address_to_object(listing))
        } ...
    }
}
```

Tradeport's adapters are one function each (`wapal_listings::buy_token(&signer, address)`),
which is the right size.

Price protection: the foreign listing's price is read on-chain via the
venue's `price` view (`coin_listing::price` exists on every reference fork)
and asserted against the caller's `expected_price` before the call, so a
seller repricing between quote and fill aborts instead of overcharging.

**Tier 2: offers and instant-sell.** Same adapter pattern for
`collection_offer::sell_tokenv2` / `token_offer::sell_tokenv2` on foreign
venues, so a holder can hit the best bid anywhere from our portfolio page.
Foreign offers are in the same normalized tables (§4.3), so "best offer" is
one query.

### 4.3 Indexing foreign venues

One Go processor (`aptos/go-backend/cmd/nft-indexer`) consumes the
Transaction Stream and applies per-venue event mappings into three current
tables plus one activity table, with the exact schema of the Aptos NFT
Aggregator (`nft_marketplace_activities`, `current_nft_marketplace_listings`,
`current_nft_marketplace_token_offers`,
`current_nft_marketplace_collection_offers`), keyed by
`(token_data_id, marketplace)` and carrying `contract_address`,
`listing_id` (the listing object address for v2 venues), `price`, `seller`,
`buyer`, `expiration_time`, `is_deleted`.

Event mappings per venue (the raw event names the processor matches; all
verified from ABIs):

| Venue | listing created / cancelled / filled | token offer placed / cancelled / filled | collection offer placed / cancelled / filled |
|---|---|---|---|
| Tradeport v2 | `listings_v2::InsertListingEvent` / `DeleteListingEvent` / `BuyEvent` | `biddings_v2::InsertTokenBidEvent` / `DeleteTokenBidEvent` / `AcceptTokenBidEvent` | `biddings_v2::InsertCollectionBidEvent` / `DeleteCollectionBidEvent` / `AcceptCollectionBidEvent` |
| Tradeport v1 | `listings::InsertListingEvent` / `DeleteListingEvent` / `BuyEvent` (plus `UpdateListingEvent`) | `biddings::Insert/Delete/AcceptTokenBidEvent` | `biddings::Insert/Delete/AcceptCollectionBidEvent` |
| Wapal | `events::ListingPlacedEvent` / `ListingCanceledEvent` / `ListingFilledEvent` | `events::TokenOfferPlacedEvent` / `...CanceledEvent` / `...FilledEvent` | `events::CollectionOfferPlacedEvent` / `...CanceledEvent` / `...FilledEvent` |
| Rarible | `events::ListingPlaced` / `ListingCanceled` / `ListingFilled` | `events::TokenOfferPlaced` / `TokenOfferCanceled` / `TokenOfferFilled` | `events::CollectionOfferPlaced` / `CollectionOfferCanceled` / `CollectionOfferFilled` |
| Bluemove v1 | `marketplaceV2::ListEvent` / `DelistEvent` / `BuyEvent` | `offer_lib::OfferEvent` / `CancelOfferEvent` / `AcceptOfferEvent` | `offer_lib::OfferCollectionEvent` / `CancelOfferCollectionEvent` / `AcceptOfferCollectionEvent` |
| Topaz v1 | `events::ListEvent` / `DelistEvent` / `BuyEvent` | `events::BidEvent` / `CancelBidEvent` / `SellEvent` | `events::CollectionBidEvent` / `CancelCollectionBidEvent` / `FillCollectionBidEvent` |
| Ours | mirror Wapal's names (reference fork) | | |

Two subtleties the aggregator's config handles and we must copy:

- Wapal/Rarible/ours carry `TokenMetadata { creator_address, collection_name, token_name, token: Option<Object<Token>>, property_version: Option<u64> }` inside the event, so the processor derives `token_data_id` for either standard from one struct. Tradeport v1 / Bluemove / Topaz emit `0x3::token::TokenId` and need the v1 hash.
- Fills on foreign venues that happen *through our router* emit both the venue's fill event and our `RouterFill`; the activity table records the venue's event as the sale and our event as attribution, never two sales.

Cursor persistence follows `eventingestor/poller.go`: the processor stores
the last fully-committed transaction version per stream and replays a batch
on crash, with idempotent upserts keyed by `(txn_version, event_index,
marketplace)`.

### 4.4 Failure handling on multi-venue sweeps

A Move transaction is atomic: if one of five listings in a sweep was sold a
block earlier, the whole `buy_many` aborts. Tradeport accepted this. Options:

1. Accept atomic all-or-nothing, and re-simulate right before signing with
   the freshest listing set (P0; simplest, matches user expectation of "the
   cart either fills or it doesn't").
2. Best-effort sweep: the router skips listings whose `price` view no longer
   matches or whose object no longer exists, and emits a `Skipped` event.
   Possible for reference-fork venues because their state is object-based
   and readable; harder for Tradeport v1 and Bluemove v1 table-based
   listings. P1.

### 4.5 What we cannot do

- We cannot list on a foreign venue on the user's behalf without them
  signing that venue's `init_fixed_price`; there is no reason to.
- We cannot collect a fee on a Tier 0 buy (no contract of ours runs).
- We cannot make Tradeport v1 or Topaz signed listings (`buy_v2` with a
  `vector<u8>` signature from Topaz's `verify` module) work if the signing
  key holder is gone; those listings are dead liquidity and should be
  indexed as cancelled if simulation fails.

## 5. Open questions and spikes before the build plan

1. **Bytecode-dependency compile.** Spike: `aptos move download` Wapal's
   `Marketplace` package and compile a one-function adapter against it on
   testnet-equivalent addresses. If this fails, Tier 1 becomes a Move script
   composed per transaction (the TS SDK Script Composer path), and the fee
   is collected by a separate `coin::transfer` in the same script.
2. **Aggregator endpoint status.** Ask Aptos Labs whether
   `nft-aggregator/v1/graphql` moved behind API keys. If it exists, it is a
   free historical backfill; if not, backfill from the Transaction Stream
   from the first Tradeport v2 transaction.
3. **Rarible and Bluemove v2 live volume.** The `account_transactions`
   table is blind to object-based venues; measure from the stream during the
   indexer spike.
4. **Fee level.** Tradeport charged a router fee with a per-wallet loyalty
   discount. Decide our aggregator fee (bps) and whether our own venue's
   commission is lower to pull listings across.
5. **Go module boundary.** Separate `aptos/go-backend` module (own CI, own
   bake context) versus adding `cmd/nft-*` services to the existing
   `go-backend` module (free reuse of `internal/platform`, but any change
   under `go-backend/**` rebuilds the Sui services). Recommendation: separate
   module, copy the four platform packages it needs.
6. **Structured logging in Go.** `tx-alerting.md` requires
   `alert_id` fields; Go services use `log.Printf`. Adopt `slog` in the new
   module from the start.
7. **Image pipeline hosting.** S3 + CloudFront versus an image-proxy service;
   budget item, not a design question.

## 6. Proposed repository layout

```
aptos/
  README.md
  contracts/
    marketplace/        # fork of aptos-core move-examples/marketplace: listing, coin_listing,
                        # fee_schedule, token_offer, collection_offer, events
    router/             # buy_many + one adapter module per foreign venue; depends on
                        # downloaded on-chain packages under router/deps/<venue>/
  go-backend/
    go.mod              # github.com/ewitulsk/SuiOptions/aptos/go-backend
    cmd/nft-indexer/    # transaction-stream processor -> Postgres (venues + ours + metadata)
    cmd/nft-api/        # public REST + WS; admin mux
    internal/
      venues/<name>/    # event mapping + payload builder per venue (tier 0 + tier 1 args)
      stream/           # gRPC transaction stream client (generated from aptos-core protos)
      store/            # pgx + goose migrations
      platform/         # copied from go-backend/internal/platform: config, db, obs, cors
.github/workflows/
  aptos-move-ci.yml     # aptos CLI: `aptos move test` per package
  aptos-go-ci.yml       # gofmt / vet / test with postgres service
docs/aptos-nft-marketplace/
  00-plan.md            # this chapter
  appendix-addresses.md
```

Frontend: a new `aptos-frontend/` Vite app (or a route tree inside
`frontend/`) using `@aptos-labs/wallet-adapter-react`; decided in chapter 01.

## 7. Quote tokens: list in any allowlisted Fungible Asset

Decision: the seller picks the quote token per listing; the contract accepts
any token on an admin-managed allowlist; every price, offer, fee and royalty
is denominated in that listing's quote token.

Why not literally "any token":

- The reference marketplace is `Coin<CoinType>`-generic
  (`coin::withdraw<CoinType>`, `aptos_account::deposit_coins`). Native USDC
  (`0xbae2...46f3b`) and USDt (`0x357b...dc2b`) on Aptos are Fungible
  Assets with **no Coin type at all**, so the reference contract cannot
  price a listing in either. Aptos has also migrated every CoinStore to a
  FungibleStore, so FA is the one payment primitive that reaches everything.
  Our fork replaces the Coin leg with `primary_fungible_store::withdraw` /
  `deposit` against an `Object<Metadata>` stored in the listing.
- The quote token becomes a **data field, not a type parameter**. One
  `list(seller, token, quote: Object<Metadata>, price)` entry, one event
  shape, one indexer path, instead of an event type per `CoinType`.
- USDC and USDt both carry dispatchable transfer hooks
  (`0x1::fungible_asset::DispatchFunctionStore` exists on both). A
  permissionless quote token means a malicious FA's hook can abort selectively
  (grief a specific buyer or our fee address), skim a transfer tax so the
  seller receives less than `price`, or freeze the store that escrows
  collection-offer funds. Allowlisting is the only cheap defence.
- Floor price and volume need a common denominator. A listing in a token the
  UI cannot price is invisible to the stats and useless to buyers.

Allowlist shape (admin-editable object, mirrors `exchange-listing`'s
per-quote-coin economics on the Sui side):

```
struct QuoteToken has store { metadata: Object<Metadata>, enabled: bool, min_fee: u64 }
```

Seed: APT (`0xa`), USDC, USDt. Add others on request. Offers and collection
offers escrow the quote FA in an object-owned fungible store, so they work
in any allowlisted token too. Foreign venues stay `Coin<T>`-generic, which is
APT in practice; the router does not change that.

## 8. Fees

| Where | Who pays | Mechanism | Level |
|---|---|---|---|
| Fill on our venue (listing bought, or our-venue offer accepted) | Seller, deducted from proceeds after royalty | `fee_schedule::commission` (percentage) in the listing's quote token, paid to the treasury at fill | P0 |
| Buy on a foreign venue through our router | Buyer, added on top of the venue's price | `router::buy_many` withdraws `price + fee` and pays the fee before calling the venue; the venue's own commission still goes to that venue | P0 |
| Instant-sell into a foreign bid through our router | Seller, from proceeds | Same adapter, fee taken from what the foreign contract pays out | P1 |
| Listing fee, bidding fee | nobody | Reference supports fixed fees at list and bid time; set to zero, they only suppress supply | never |
| Launchpad mints, featured placement, API keys, gas sponsorship markup | | Non-trading revenue; not in scope now | P2 |

Mechanics worth copying from Tradeport: a `min_fee` floor per quote token so
dust trades do not produce dust fees (`update_min_fee_amount`), per-collection
overrides for partner and zero-fee deals (`fees::upsert_collection_numerator`),
and per-wallet loyalty discounts (`loyalty::add_per_wallet_numerators`). In
the reference design a listing references a `FeeSchedule` object at list
time, so rate changes are made by mutating that shared object rather than by
relisting. Fees accrue in whatever quote token the trade used; the treasury
holds a basket, and there is no on-chain swap.

## Revision history

- 2026-09-02: initial landscape, feature cut, and cross-venue design.
- 2026-09-02: added quote-token allowlist decision (§7) and fee placement (§8).
