# 02 — On-Chain RFQ (`rfq.move`)

**Files**: new `contracts/sources/rfq.move`, additions to `events.move`, `errors.move`
**Depends on**: 01 (write core)
**Blocks**: 03 (vault), 05 (mm-bot bidder, indexer)

## 1. What it is

An on-chain, per-slice premium auction that fills the role the off-chain signed-quote RFQ
fills for retail, but with no off-chain trust at all:

> Vault (or any seller) escrows underlying and opens an RFQ → `RfqCreated` event → MMs
> submit **escrowed** premium bids on-chain → after the deadline, **anyone** settles: the
> contract executes the write against the best bid.

Design choices and why:

- **Open ascending auction with escrowed bids**, not signed quotes. On-chain selection of a
  "best quote" only works if the quote is guaranteed executable; a signed quote backed by an
  `Account` balance can be invalidated by a withdrawal between selection and execution.
  Escrowing the bid coin makes the best bid *always* settleable, which is what makes the
  keeper permissionless.
- **Not sealed-bid** (commit–reveal) for v1: two extra phases, capital locked through reveal,
  and a no-show griefing surface. The anti-snipe extension (§3.3) recovers most of the price
  competition a blind auction would give. Revisit post-MVP if MM feedback demands it.
- **Per-slice objects**: the seller decides slice sizes; each slice is an independent shared
  `RfqAuction` object, so slices parallelize across MMs and a failed slice doesn't poison the
  round.

## 2. Types

```move
module options_protocol::rfq;

public struct RfqAuction<phantom Underlying, phantom Settlement, phantom Call> has key {
    id: UID,
    bucket_id: ID,
    /// Underlying escrowed for this slice; written into the bucket at settle.
    underlying: Balance<Underlying>,
    amount: u64,                       // == underlying.value(), cached for reads
    /// Total premium floor for the slice (settlement smallest-units). Bids
    /// below this are rejected. Set by the seller (the vault derives it
    /// from Pyth — doc 03 §6).
    reserve_premium: u64,
    created_ms: u64,
    deadline_ms: u64,
    /// Anti-snipe: a new best bid inside `snipe_window_ms` of the deadline
    /// pushes the deadline out by `snipe_extension_ms`, capped at
    /// `max_deadline_ms`.
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_deadline_ms: u64,
    /// Minimum improvement over the current best, in bps of the best bid.
    min_increment_bps: u64,
    /// Current best bid (premium escrowed in `bid_escrow`).
    best_bidder: Option<address>,
    best_call_recipient: Option<address>,
    bid_escrow: Balance<Settlement>,
    /// Where settle() sends the outputs. For a vault these are the vault
    /// object's ID-as-address; the vault-coupled settle path (doc 03 §7.5)
    /// bypasses the transfers and absorbs directly.
    position_recipient: address,
    proceeds_recipient: address,
    /// Originating object (vault ID, or seller address for standalone use).
    /// Indexing/attribution only.
    origin: ID,
}
```

Notes:

- `RfqAuction` is a **shared object** (bids mutate it from many parties). Settle consumes it
  by value — Sui supports deleting shared objects in a transaction (same pattern as
  `cleanup_bucket`).
- `best_premium` is `bid_escrow.value()` — no duplicate field to drift.

## 3. Functions

### 3.1 `create`

```move
public fun create<U, S, C>(
    bucket: &Bucket<U, S, C>,
    underlying: Coin<U>,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    position_recipient: address,
    proceeds_recipient: address,
    origin: ID,
    clock: &Clock,
    ctx: &mut TxContext,
): ID    // returns the auction object ID; object is shared
```

Checks:

- `underlying.value() > 0` (`zero_amount`).
- `!bucket.invalidated` and `now < bucket.expiry_ms` (`bucket_invalidated` / `bucket_expired`).
- **Settle-before-expiry**: `max_deadline = now + duration_ms + max_extension_ms` must satisfy
  `max_deadline + SETTLE_BUFFER_MS ≤ bucket.expiry_ms`, where `SETTLE_BUFFER_MS` (e.g.
  10 minutes, module constant) guarantees the settle crank can land while the bucket still
  accepts writes. Error: new `rfq_too_close_to_expiry`.
- `duration_ms ≥ MIN_DURATION_MS` (e.g. 5 minutes) so MMs can react. Error: `rfq_duration_too_short`.

Emits `RfqCreated` (§4). This is the event the mm-bot bidder subscribes to (doc 05 §3).

`create` is deliberately **public and seller-agnostic** — the vault is just one caller. Any
holder of underlying can auction a covered write (this also subsumes the Ribbon-style
"mint-then-auction" path: it's the same mechanism with the write deferred to settle).

### 3.2 `bid`

```move
public fun bid<U, S, C>(
    rfq: &mut RfqAuction<U, S, C>,
    premium_in: Coin<S>,
    call_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

Logic:

1. `now < rfq.deadline_ms` (`rfq_closed`).
2. `value = premium_in.value()`; require
   `value ≥ max(reserve_premium, best × (10_000 + min_increment_bps) / 10_000)`
   (u128 intermediate; error `rfq_bid_too_low`). When there is no best bid, the floor is
   just `reserve_premium`.
3. Refund the previous best bid, if any: withdraw all of `bid_escrow` into a `Coin<S>` and
   `public_transfer` to the previous `best_bidder`. (Push refunds are safe on Sui — a
   transfer to an address cannot fail or re-enter.)
4. Escrow `premium_in` into `bid_escrow`; set `best_bidder = sender`,
   `best_call_recipient = call_recipient`.
5. Anti-snipe: if `rfq.deadline_ms - now < snipe_window_ms`, set
   `deadline_ms = min(now + snipe_extension_ms, max_deadline_ms)`.
6. Emit `RfqBid`.

MMs fund bids from their wallet, or from their protocol `Account` in the same PTB via
`account::withdraw<S>` (sender is the account owner, so the owner check passes).

### 3.3 Anti-snipe rationale

Open auctions on-chain invite deadline sniping (bid at the last block, deny others the chance
to respond). The extension converts a snipe into a Vickrey-like price war while the cap
(`max_deadline_ms`, which also feeds the settle-before-expiry check) bounds the total
duration. `min_increment_bps` prevents 1-unit-increment spam from riding the extension
forever. Suggested defaults: window 60 s, extension 120 s, cap = deadline + 15 min,
increment 50 bps.

### 3.4 `settle` — the generic path

```move
public fun settle<U, S, C>(
    rfq: RfqAuction<U, S, C>,            // consumed
    bucket: &mut Bucket<U, S, C>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

Logic:

1. `now ≥ rfq.deadline_ms` (`rfq_not_closed`).
2. `rfq.bucket_id == object::id(bucket)` (`rfq_bucket_mismatch`).
3. **Winner exists** (`best_bidder.is_some()`):
   a. `(net, fee) = bucket::skim_fee(config, treasury, bid_escrow)` — protocol fee applies to
      on-chain RFQ premiums exactly as to signed-quote premiums.
   b. `(position, call) = bucket::write_collateralized_balance(bucket, underlying, clock, ctx)`
      — a `Balance`-accepting sibling of `write_collateralized` (same checks; add it in
      doc 01's change set or convert here via `coin::from_balance`).
   c. `public_transfer(call, best_call_recipient)`;
      `public_transfer(position, rfq.position_recipient)`;
      `public_transfer(coin::from_balance(net), rfq.proceeds_recipient)`.
   d. Emit `RfqSettled` with the position id, range, winner, gross/fee/net premium.
4. **No winner**: return the escrowed underlying to `proceeds_recipient` as a `Coin<U>`;
   emit `RfqExpiredUnsold`.
5. Delete the auction object.

Settle is callable by **anyone** — there is nothing to steal; all outputs go to addresses
fixed at creation (or to the winner).

### 3.5 `settle_internal` — the vault-coupled path

The generic path transfers the `Position` and premium to the vault's ID-as-address, which
would force the vault to retrieve them with `transfer::receiving` cranks. To keep the common
case atomic, expose a package-internal finalizer the vault module (doc 03 §7.5) calls inside
its own `settle_rfq`:

```move
/// Returns (winner-side outputs) to the calling module instead of
/// transferring. `None` ⇒ auction failed; the Balance<U> is the refunded
/// collateral. `Some` ⇒ (call_recipient, gross premium escrow, collateral
/// to be written).
public(package) fun finalize<U, S, C>(
    rfq: RfqAuction<U, S, C>,
    clock: &Clock,
): (Option<FinalizedBid<S>>, Balance<U>, RfqReceipt)
```

where `FinalizedBid<S> { bidder: address, call_recipient: address, premium: Balance<S> }` and
`RfqReceipt` carries the ids/params for event emission by the caller. The vault then performs
fee skim + write + call transfer itself and deposits the premium and `Position` directly into
its own state — one transaction, no receiving dance.

(Implementation detail: keep `settle` itself implemented on top of `finalize` so the two
paths cannot diverge.)

## 4. Events (`events.move` additions)

```move
public struct RfqCreated has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
}

public struct RfqBid has copy, drop {
    rfq_id: ID,
    bidder: address,
    call_recipient: address,
    premium: u64,
    previous_premium: u64,        // 0 if first bid
    new_deadline_ms: u64,         // post-anti-snipe deadline
}

public struct RfqSettled has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    call_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

public struct RfqExpiredUnsold has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}
```

`RfqSettled` intentionally mirrors `WriteExecuted`'s economic fields so the indexer's
`positions` materializer can treat both as "a position was minted with premium X"
(doc 05 §1).

## 5. New error codes (`errors.move`)

```move
public fun rfq_closed(): u64 { 29 }
public fun rfq_not_closed(): u64 { 30 }
public fun rfq_bid_too_low(): u64 { 31 }
public fun rfq_bucket_mismatch(): u64 { 32 }
public fun rfq_too_close_to_expiry(): u64 { 33 }
public fun rfq_duration_too_short(): u64 { 34 }
```

## 6. Attack / griefing analysis

| Scenario | Outcome |
|---|---|
| Lone colluding bidder bids the reserve and nothing more | Seller receives `reserve_premium`. The reserve is therefore the **only** price-safety floor — the vault derives it from Pyth (doc 03 §6) so a quiet auction cannot leak value below the floor. |
| Bidder spam with tiny increments near deadline | Blocked by `min_increment_bps` + `max_deadline_ms` cap. Each failed bid costs the spammer gas; refunds are push-based and cannot be blocked. |
| Winner never settles | Anyone can settle; the keeper does it; the winner has escrowed funds at stake and *wants* settlement. A permanently unsettled auction (no one cranks) keeps everyone's escrow locked but is recoverable by any party at any time — no deadline on `settle`. |
| Settle racing the bucket expiry | Prevented at creation by the `SETTLE_BUFFER_MS` check. If somehow settle still lands post-expiry, `write_collateralized` aborts with `bucket_expired`; add a `settle_expired` variant that refunds both sides (bid → bidder, underlying → proceeds_recipient) when `now ≥ bucket.expiry_ms`, so funds can never be stranded. |
| Bucket invalidated mid-auction | `write_collateralized` aborts at settle. Same recovery: `settle_expired`-style refund path also accepts `bucket.invalidated == true`. |
| Refund-to-contract grief (bidder is an object address) | Refund is `public_transfer` of a `Coin<S>` to an address — always succeeds on Sui; coins sent to object addresses are recoverable by the object owner via receiving. The auction does not care. |
| Front-running a bid | A higher bid is the only way to "front-run"; that is the auction working. |

## 7. Step-by-step

1. Implement `rfq.move` with `create`, `bid`, `finalize`, `settle`, `settle_expired`, getters.
2. Add the four events + emit helpers; add error codes.
3. Tests (`contracts/tests/rfq_tests.move`):
   - Full lifecycle: create → 3 bids (assert refunds land back with the outbid parties) →
     settle → winner holds `Coin<Call>` of the slice size, position recipient holds the
     `Position` with the correct range, proceeds recipient holds net premium, treasury holds
     the fee, bucket cursor/supply consistent.
   - Reserve enforcement; min-increment enforcement; anti-snipe extension & cap; settle
     before deadline aborts; double-settle impossible (object consumed).
   - No-bid expiry: underlying refunded; `RfqExpiredUnsold` emitted.
   - `settle_expired`: bucket past expiry / invalidated ⇒ both escrows refunded.
   - Interleaving property test: signed-quote writes, collateralized writes, and RFQ settles
     into one bucket; assert `total_written`, call supply, and balances stay consistent and
     all positions redeem exactly.
4. `sui-tx` builders for create/bid/settle (consumed by mm-bot and keeper — doc 05 §5).
