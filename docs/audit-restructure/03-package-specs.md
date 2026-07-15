# Package Specifications — Audit Reference

Companion to `01-onchain-plan.md` (the plan this implements) and the
protocol spec (`options-protocol-spec.md`, which covers the core
covered-call primitives in depth). This document specifies what the
four-package split *added or changed*: the package trust model, the
generic auction, the option-RFQ adapters, the cash-secured-put math, and
the reviewed public surface of core. Where this doc and the older
`vault-implementation-guide` disagree on module *names* (`rfq.move`,
`swap_auction.move`), this doc is current — the vault guide's economics
and state machine are unchanged.

## 1. Package architecture and trust model

```
options_core   ◄── options_rfq ◄─┐        (arrows = "depends on")
auction        ◄─────┴───────── options_vault
```

| Package | Third-party deps | Holds funds | Trusted by |
|---|---|---|---|
| `options_core` | none | all bucket collateral, accounts, treasury | everyone |
| `auction` | none | live auction escrows + best bids | its coupled venues |
| `options_rfq` | none | nothing (flows through in one tx) | nobody (stateless adapters) |
| `options_vault` | pyth | vault AUM, receipts | its depositors |

Design rules the split enforces:

1. **Core knows nothing above it.** No core module references auctions or
   vaults. Its collateralized-write surface is public and
   permissionless-safe (§5), so upper layers need no privileged access.
2. **The auction machine knows nothing about options.** No bucket types,
   no oracle, no settlement semantics — one escrowed ascending auction
   with a witness-typed settle authority (§2).
3. **Authority is type-level, not id-level.** A coupled auction records
   the `TypeName` of a witness `W`; only the module that can construct
   `W` can finalize it. Forging an auction with a vault's `origin` id is
   possible (origin is caller-supplied), but useless: the vault's settle
   path finalizes with `VaultAuth`, and a foreign auction's settle
   authority won't match (`not_settle_authority`, tested).

## 2. `auction` — generic escrowed ascending auction

One shared object per auction:
`Auction<Escrow, Bid>` escrows the asset being sold and the current best
bid. Key invariants:

- **Escrowed bids make settle permissionless.** The best bid's funds are
  always in the object (`bid_escrow.value()` IS the best bid), so any
  crank can settle after the deadline; nothing depends on the winner
  showing up. Outbid bidders are refunded by push transfer in the same
  `bid` call.
- **Reserve floor**: bids `< reserve_bid` rejected — the only
  price-safety a quiet auction has; the creator derives it (the vault
  from Pyth, a standalone seller however they like).
- **Strict min-increment**: a new best must be `> best` AND
  `≥ ceil(best × (1 + min_increment_bps/10⁴))`, floored at the reserve.
- **Anti-snipe**: a best bid landing within `snipe_window_ms` of the
  deadline pushes the deadline to `now + snipe_extension_ms`, capped at
  `max_deadline_ms` (fixed at creation as `deadline + max_extension`).
- **Minimum duration** 300s so bidders can react to `AuctionCreated`.

Two creation paths:

- `create` (public, uncoupled): anyone can run a swap auction. Outcome
  routing is fixed at creation: winner's `token_recipient` gets the
  escrow, `proceeds_recipient` gets the winning bid, `refund_recipient`
  gets the escrow back if unfilled. Resolved by the public `settle`.
- `create_coupled<W: drop>` (witness): records `TypeName<W>` as the
  settle authority. Only `finalize<W>` / `finalize_early<W>` can resolve
  it, returning the raw balances as hot potatoes the venue must absorb
  in the same transaction. The public `settle` rejects coupled auctions
  (`settle_coupled`) — otherwise it would strand a venue's outputs at
  its id-as-address.
- `finalize_early<W>` has **no deadline precondition**: it exists for
  venues whose external coupling can die mid-auction (a bucket expiring
  or being invalidated). The venue's own preconditions gate legitimacy;
  bidders accept this when they bid on a coupled auction — the authority
  module's code is the contract.

Audit surfaces: increment/reserve floor math (u128 ceiling division),
deadline extension capping, the witness comparison
(`type_name::with_defining_ids`), balance conservation through
`destroy`, and that every resolution path either transfers or returns
both escrows.

## 3. `options_rfq` — option-RFQ adapters

Each option RFQ is a **pair** of shared objects: the generic
`Auction<Escrow, Settlement>` (owned by the machine) and a typed metadata
object here binding it to a bucket and payout routing:

- Covered call: `CallRfq<U,S,C>` + `Auction<U, S>` — escrow is
  underlying, bids are settlement premium.
- Cash-secured put: `PutRfq<U,S,P>` + `Auction<S, S>` — escrow is the
  exact cash collateral `required_collateral(bucket, amount)` (checked at
  create), bids are settlement premium. `amount` (the option notional in
  underlying units) lives on the metadata, not the auction.

The adapter holds the auctions' settle authority (`RfqAuth`). Settle
paths (permissionless cranks):

- `settle_call` / `settle_put`: finalize (deadline passed), then on a
  winner: `skim_fee` → `write_collateralized_balance` → option coins to
  the winner's `token_recipient`, `Position` to `position_recipient`,
  net premium to `proceeds_recipient`. No winner: escrow refunded to
  `proceeds_recipient`.
- `settle_call_expired` / `settle_put_expired`: recovery when the bucket
  expired or was invalidated mid-auction (asserted here) — uses
  `finalize_early`, refunds the standing bid to the bidder and the
  escrow to `proceeds_recipient`. No funds can strand.

Both settle paths assert `rfq.auction_id == id(auction)` and
`rfq.bucket_id == id(bucket)` — a metadata object can only resolve its
own auction against its own bucket.

Create-side bucket preconditions: not invalidated, not expired, and
`now + duration + max_extension + SETTLE_BUFFER (10 min) ≤ expiry` so the
settle crank always has room to land while the bucket still accepts
writes.

## 4. Cash-secured puts (core: `put_bucket`)

The put is the collateral-mirrored twin of the call. Same
pooled-bucket/FIFO-cursor model; the differences:

- **Collateral is settlement cash**: writing `amount` underlying-units
  requires `required_collateral = ceil(amount × strike / 10^strike_scale)`
  — ceiling, so the bucket can never be under-collateralized by
  rounding.
- **Exercise delivers underlying**: the holder burns `Coin<Put>` and
  delivers `amount` underlying, receiving
  `floor(amount × strike / 10^scale)` cash — floor, against the
  exerciser.
- **Redemption**: an assigned writer receives the delivered underlying;
  unassigned collateral returns as `floor(unexercised × strike / 10^scale)`
  cash. The ceil-at-write / floor-at-exit asymmetry leaves rounding dust
  in the bucket, swept to the admin at `cleanup` (`PutBucketCleaned.dust_swept`).
- The write cursor advances in **underlying units** (the notional), not
  collateral units — exercise/redeem range math is identical to calls.

## 5. Reviewed public surface of `options_core`

Promoted from `public(package)` during the split, each reviewed
permissionless-safe:

| Function | Why safe public |
|---|---|
| `bucket::write_collateralized_balance` | Full collateral in, `Position` + option coin out 1:1. No premium leg, no quote bypass; supply == collateral preserved. (The `Coin`-accepting `write_collateralized` was already public.) |
| `put_bucket::write_collateralized_balance` | Same, with the exact-cash-collateral check inside. |
| `bucket::skim_fee` | Splits the configured fee into the Treasury; an outside caller can only donate. |
| `treasury::deposit_balance` | Deposit-only. |
| `bucket::required_settlement`, `put_bucket::required_collateral` | Read-only strike math. |

Everything else keeps its original visibility; quote verification,
account debits, and the executor write path (`execute_write`) are
unchanged from the v0.1 spec.

## 6. `options_vault` — what changed in the split

Vault economics, the round state machine, fees, and receipt accounting
are exactly as specified in `docs/vault-implementation-guide/03`. The
split changed only the auction plumbing:

- The vault couples **directly** to the generic auction with its own
  `VaultAuth` witness — for both RFQ slices (`Auction<U, S>`) and
  proceeds swaps (`Auction<S, U>`; note the legs are reversed vs the old
  `SwapAuction<U, S>`). It does not depend on `options_rfq`.
- `settle_rfq` verifies `origin == vault id` AND finalizes with
  `VaultAuth` (the origin check alone would be forgeable; the witness is
  the real gate). Winner path: fee skim → collateralized write →
  position absorbed into the vault FIFO, net premium into proceeds.
- `settle_swap_rfq` re-checks the winning rate against a **fresh** Pyth
  cross at settle (band = `max_swap_slippage_bps`); an out-of-band or
  empty auction returns the settlement to proceeds for re-auction and
  refunds the bidder. The winner's settlement now routes to the bid's
  `token_recipient` (previously the bidder address).
- `settle_rfq_expired` asserts the bucket is dead (expired/invalidated)
  and uses `finalize_early` — the recovery invariant (no admin needed to
  unstick a round) is preserved.
- Events: `VaultRfqSettled` / `VaultRfqUnsold` (from this package)
  replace the old vault-emitted `RfqSettled` / `RfqExpiredUnsold`;
  auction creations and bids surface as the generic `AuctionCreated` /
  `AuctionBid`. `SwapRfqSettled` / `SwapRfqUnfilled` are unchanged in
  shape. Vault error codes keep their historical values (35–54).

## 6b. Collateral abstraction addendum (v0.3)

Implemented after this document's first revision; normative spec in
`04-collateral-abstraction-plan.md`. Summary of what changed in
`options_core`'s audit surface:

- `Account` → `QuoteSigner`: signing key + consumed-nonce table only.
  Core custodies NO market-maker funds (buckets/treasury still custody
  protocol funds).
- `collateral::CollateralRequest<T>`: a no-abilities hot potato minted
  by `bucket::request_{writer,trader}_flow` (and put twins) after full
  quote verification + nonce consumption. Carries the verified quote,
  the demanded amount, and a flow tag (writer/trader) so a
  premium-sized request can't be routed into the trader execute path —
  load-bearing for puts, where both legs are the settlement asset.
- `execute_{writer,trader}_flow` consume the potato + the released
  `Balance` and run the write; amount/type/bucket binding re-checked.
  Atomicity is structural: the potato can't be dropped or stored.
- The quote's collateral routing (`collateral_source`,
  `release_package`, `release_module`) is INSIDE the signed BCS
  payload; `origin`-style unsigned hints do not exist here.
- The release convention (`release<T>(account, &request, ctx):
  Balance<T>`, must assert `source(request) == id(account)`) is
  enforced socially + by the gas station's wildcard template shape, not
  by core; a malicious implementation can only abort. First-party
  implementation: `contracts/mm-collateral` (~150 lines, one MM per
  published copy, publisher-owned).

## 7. Known accepted behaviors (flag for auditors, not bugs)

- **Adversarial auction creators** exist by design (public `create`).
  Bidders' funds are only ever in `bid_escrow` and are refunded on
  outbid, unfilled settle, or early finalize; a malicious creator can
  waste a bidder's time, not their funds. A coupled venue's
  `finalize_early` can cut an auction short — bidders on coupled
  auctions trust the venue module's preconditions (bucket-death only, in
  every shipped venue).
- **Anyone can mint options at full collateral** via the public
  collateralized-write surface. This is economically a premium-free
  covered write; it cannot dilute or extract from other writers (the
  cursor model prices every range identically at redemption).
- **`origin` is attribution, not authorization** — everywhere.
  Authorization is the witness.
