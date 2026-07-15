# Audit Restructure — On-Chain Plan

Prep the Move contracts for audit and production: scrap the session-token
additions, then split the monolithic `options_protocol` package into four
packages with clean, one-way, auditable boundaries — mirroring the Solana
port's `auction_venue` architecture (generic auction machine + option
adapters, commit `8273eca`), so both chains share one spec and one mental
model.

## Target package layout

```
options_core     bucket, put_bucket, account, quote, position, admin,
                 treasury (+ own events/errors)
                 → ZERO third-party dependencies (pure Sui framework)

auction          generic escrowed ascending auction: the standalone swap
                 product AND the general on-chain RFQ machine
                 → ZERO dependencies (leaf package)

options_rfq      call/put auction adapters: create option RFQs on the
                 generic venue, settle them into buckets
                 → depends on options_core + auction

options_vault    vault + oracle
                 → depends on options_core + auction + pyth
```

> Implementation note: the vault couples **directly** to the generic
> auction with its own `VaultAuth` witness and performs the bucket write
> itself (as it always did in `settle_rfq`), so `options_vault` does NOT
> depend on `options_rfq` — the adapters serve only standalone option
> RFQs. The vault emits `VaultRfqSettled`/`VaultRfqUnsold` from its own
> events module.

Dependency DAG (all one-way, no cycles — this already matches today's
module-level `use` graph):

```
options_core ◄── options_rfq ◄── options_vault
auction      ◄──────┴───────────────────┘
```

Audit story per package: `options_core` custodies all user funds and holds
the cursor math — deepest review, zero external deps. `auction` is a small
(~400 line) self-contained primitive with no deps. `options_rfq` is thin
glue. `options_vault` quarantines the pyth dependency and the accounting
complexity.

---

## Phase A — Scrap session (own PR, first)

Removes ~1,150 lines of source, three vendored/local dependencies, and the
whole session trust surface before anything else is touched.

1. Delete modules: `session_account.move`, `session_bucket.move`,
   `session_put_bucket.move`, `session_vault.move`, `session_deepbook.move`.
2. Delete tests: `session_deepbook_tests.move`,
   `session_integration_tests.move`, `session_put_tests.move`.
3. `Move.toml`: drop `siws_session`, `deepbook`, `token` deps and the
   `vendor/deepbookv3` tree — `session_deepbook` was the only contract-side
   consumer of DeepBook.
4. Remove the orphans this creates in core modules:
   - `account.move`: `new_with_owner`, `share_account`, `uid`, `uid_mut`,
     and the position/object custody helpers (`store_position`,
     `take_position`, `store_object`, `take_object` live in
     session_account and go with it).
   - `events.move`: `AccountPositionDeposit`, `AccountPositionWithdraw`,
     `AccountObjectDeposit`, `AccountObjectWithdraw` + their emit helpers.
   - `errors.move`: `session_mismatch()`.
   - Doc comments referencing session flows in `account.move` (lines 55,
     79, 195) and `bucket.move` (line 95).
5. Verify: `sui move build` + full test suite green; `grep -ri session
   sources/ tests/` returns nothing.

## Phase B — `auction` package (generic venue)

New package. Port the Solana `auction_venue` design (state at
`solana-contracts/programs/auction_venue/src/state.rs`) into Move. One
auction machine subsumes `rfq.move`, `rfq_put.move`, and
`swap_auction.move` — same mechanics, different settlement disposition.

### Core type

```move
public struct Auction<phantom Escrow, phantom Bid> has key {
    id: UID,
    creator: address,
    escrow: Balance<Escrow>,          // what's being sold
    reserve_bid: u64,                 // bids below this rejected
    min_increment_bps: u64,           // strict improvement over best
    deadline_ms: u64,
    snipe_window_ms: u64,             // anti-snipe: best bid inside the
    snipe_extension_ms: u64,          // window pushes deadline out,
    max_deadline_ms: u64,             // capped at max_deadline_ms
    bid_escrow: Balance<Bid>,         // the escrowed best bid IS the bid
    best_bidder: Option<address>,
    best_token_recipient: Option<address>,
    proceeds_recipient: address,      // fixed at creation
    refund_recipient: address,        // no-winner / refund path
    settle_authority: Option<TypeName>, // witness gate (see below)
}
```

Escrowed bids are load-bearing: the winning bid is always on hand, which
is what makes settle permissionless (any crank can settle; no dependence
on the winner showing up). Outbid → previous best refunded inline.

### Functions

- `create<E, B>(escrow: Coin<E>, params, ctx): ID` — public, permissionless.
  Anyone can run a swap auction. Enforce `MIN_DURATION_MS` (300s, matches
  Solana) so bidders can react to the creation event.
- `create_coupled<E, B, W: drop>(_: W, escrow, params, ctx): ID` — records
  `type_name::get<W>()` as `settle_authority`. This is the Move analog of
  Solana's `settle_authority` PDA: only a caller who can produce the
  witness `W` may finalize. Used by `options_rfq` and `options_vault`.
- `bid<E, B>(auction, bid: Coin<B>, token_recipient, clock, ctx)` — public.
  Reserve floor, min-increment, anti-snipe extension, refund the outbid.
- `settle_swap<E, B>(auction, clock, ctx)` — permissionless, only for
  uncoupled auctions: escrow → winner, bid → proceeds_recipient; no winner
  → escrow → refund_recipient.
- `finalize<E, B, W: drop>(_: W, auction, clock)
     : (Balance<E>, Option<FinalizedBid<B>>, AuctionReceipt)` — witness-
  gated hot-potato finalize for coupled auctions. The consumer (adapter or
  vault) receives the escrow, the winning bid (if any), and a `copy, drop`
  receipt with amounts/ids, and MUST dispose of both balances in the same
  PTB. `force_refund: bool` param ports the coupled venue's oracle-band
  veto: return escrow to refund_recipient, refund the standing bid.
- `settle_expired` — recovery path past `max_deadline_ms + buffer`.

### Events (own module)

`AuctionCreated`, `AuctionBid`, `AuctionSettled`, `AuctionUnfilled` —
generic fields only (ids, asset TypeNames, amounts, recipients). No
options vocabulary in this package.

### Verify

Port the venue test matrix from
`solana-contracts/programs/auction_venue/tests/venue_tests.rs` (reserve
rejection, increment rejection, anti-snipe extension + cap, outbid refund,
no-winner refund, coupled-witness enforcement, force_refund, expired
recovery).

## Phase C — Core surface promotion

`options_rfq` needs two `public(package)` core functions across the new
package boundary. Promote to `public` — with an explicit safety review as
an audit deliverable:

- `bucket::write_collateralized_balance` / `put_bucket::
  write_collateralized_balance` — permissionless-safe by construction:
  full collateral in, Position + option coin out 1:1. No premium leg, no
  quote bypass; supply == collateral invariant preserved. (This is the
  same cut Solana made: "payer/writer split added to core so the auction
  PDA can be the CPI writer" — core exposes an audited collateralized-
  write surface for any caller.)
- `bucket::skim_fee` — fee always routed to Treasury; a stranger calling
  it can only donate fees.
- `put_bucket::required_collateral` (read-only helper) and
  `bucket::required_settlement` as needed by the adapters.

Everything else in core keeps its current visibility. Document the
reviewed-public surface in the spec (Phase F).

## Phase D — `options_rfq` package (adapters)

Thin wrappers replacing today's `rfq.move` (494 lines) and `rfq_put.move`
(463 lines) — which are near-mirrors and a standing drift risk. The
adapters carry the options-specific ends; the machine is Phase B.

- `create_call_auction<U, S, C>(bucket, collateral: Coin<U>, params, …)` —
  validates bucket not expired/invalidated, `deadline + SETTLE_BUFFER_MS
  <= expiry` (port from rfq.move:184-192), then
  `auction::create_coupled<U, S, RfqAuth>(RfqAuth {}, …)`.
- `create_put_auction<U, S, P>(bucket, collateral: Coin<S>, params, …)` —
  same, collateral checked against `put_bucket::required_collateral`.
- `settle_call` / `settle_put` — permissionless crank: calls
  `auction::finalize<…, RfqAuth>`, then on a winner: `bucket::skim_fee` +
  `bucket::write_collateralized_balance`, Position →
  `position_recipient`, option coins → winner's `token_recipient`, net
  premium → `proceeds_recipient`. No winner: collateral back to
  `refund_recipient`.
- Vault coupling: adapters also expose `create_call_auction_coupled<…, W>`
  so the vault's own witness gates *its* auctions' settlement and routing
  (two-level authority: vault witness → adapter → generic venue).
- Events: `OptionAuctionSettled` (with range_start/range_end, premium,
  fee), `OptionAuctionUnfilled` — the options-flavored settlement facts
  the indexer needs beyond the generic venue events.

Semantics stay fixed: single winner, full amount, ascending best-bid. No
partial fills / multi-winner — that is post-audit product work.

## Phase E — `options_vault` package

- Move `vault.move` and `oracle.move` here; pyth dependency lives only in
  this package's Move.toml.
- Rewire `open_rfq`/`settle_rfq` to the `options_rfq` adapters,
  `open_swap_rfq`/`settle_swap_rfq` to `auction::create_coupled`/
  `finalize` with the vault's witness. The Pyth-bounded reserve
  computation stays in the vault (oracle logic never enters the auction
  package). `force_refund` is the vault's oracle-band veto on swap
  settlement.
- Vault events move to this package's events module unchanged.

## Phase F — Cross-cutting

- **Events/errors split**: each package gets its own `events.move` /
  `errors.move`. Event *type strings* change package + module for
  everything (off-chain plan covers decoding). Vault/bucket/account event
  shapes stay identical where possible to minimize indexer churn.
- **Tests**: redistribute per package. `e2e_tests.move` exercises only
  core flows and stays in `options_core`; each downstream package carries
  its own `test_helpers` copy (`#[test_only]` code isn't importable
  across packages, though test-only *framework* helpers are).
- **Spec parity**: `options-protocol-spec.md` is v0.1 and covers calls
  only. Write one spec section (or doc) per package — puts collateral
  math, the generic auction semantics + witness trust model, adapter
  settlement, vault accounting — before the audit starts. Auditors diff
  spec against code on day one.
- **Directory shape**: `contracts/` becomes four package dirs
  (`contracts/core`, `contracts/auction`, `contracts/rfq`,
  `contracts/vault`), each with its own `Move.toml`; local named deps
  point down the DAG.

## PR sequencing

1. **PR 1 — session scrap** (Phase A). Small, independent, shrinks
   everything downstream.
2. **PR 2 — auction package + core promotion** (Phases B + C). The generic
   venue can land while `rfq.move` still exists; nothing references it yet.
3. **PR 3 — package split + adapters + vault rewire** (Phases D + E + F).
   The big one: deletes rfq/rfq_put/swap_auction, splits events/errors,
   moves vault/oracle.
4. **PR 4 — spec docs** (Phase F spec work; can run in parallel with 3).

Staging keeps working until a redeploy: none of these PRs breaks the
running deployment. Do NOT redeploy staging between PR 3 and the off-chain
rewire (see `02-offchain-plan.md`). Any positions custodied inside session
accounts on staging are stranded on the fresh deployment — acceptable for
audit prep.

## Success criteria

- All four packages build independently; full test suite green per package.
- `grep -ri session contracts/` → empty; `options_core` and `auction`
  Move.tomls list no third-party dependencies.
- Venue test matrix (Phase B) passes; e2e vault round-trip passes against
  the adapter path.
- Sui and Solana expose the same conceptual interface (create/bid/
  settle_{swap,call,put}/settle_expired, settle-authority coupling,
  force_refund) — diffable spec.
