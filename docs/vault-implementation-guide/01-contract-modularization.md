# 01 — Contract Modularization: a composable write core

**Files**: `contracts/sources/bucket.move`, `contracts/sources/errors.move`,
`contracts/sources/events.move`
**Depends on**: nothing
**Blocks**: 02 (on-chain RFQ), 03 (vault)

## 1. Problem

`bucket::execute_write_with_quote` (`bucket.move:210`) currently fuses four concerns into one
private function:

1. **Quote validation** — bucket match, recipient match, amount match against the `Quote`.
2. **Premium routing** — flow-dependent debits from the signer `Account`, fee skim, premium
   transfer to `ctx.sender()`.
3. **Core write mechanics** — escrow underlying, advance `total_written`, mint `Position`,
   mint `Coin<Call>`.
4. **Transfers** — `Position` and `Coin<Call>` pushed to recipient addresses.

Two of those are venue-specific (1, 2), one is protocol-core (3), and one (4) actively blocks
composability: a shared object like the vault can never be `ctx.sender()`, and pushing the
net-premium coin to the sender means a vault's premium would land in the keeper's wallet.

The on-chain RFQ (doc 02) and the vault (doc 03) both need (3) without (1), (2), or (4).

## 2. Design

### 2.1 New public core: `write_collateralized`

```move
/// Core covered-write: escrow `underlying_in` in the bucket and mint the
/// corresponding `Position` + `Coin<Call>`, RETURNED to the caller (no
/// transfers). Premium negotiation is a venue-layer concern.
///
/// Safety argument for being `public`: this mints no free optionality.
/// The caller fully collateralizes every option unit minted, and until
/// they part with the `Coin<Call>` they hold both sides of the trade —
/// economically a no-op. It is exactly the "self-write" primitive that
/// lets anyone build a venue (auction, AMM listing, OTC) on top of the
/// protocol.
public fun write_collateralized<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    underlying_in: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    let amount = underlying_in.value();
    assert!(amount > 0, errors::zero_amount());
    let (position, call) = do_write(bucket, underlying_in.into_balance(), ctx);
    events::emit_collateralized_write(
        object::id(bucket),
        ctx.sender(),
        amount,
        position::range_start(&position),
        position::range_end(&position),
    );
    (position, call)
}
```

### 2.2 Private mechanics: `do_write`

Extract steps 8–10 of the current function verbatim (cursor assignment + both mints) into:

```move
/// Bucket mechanics shared by every write venue. Caller has already
/// performed venue checks (expiry, invalidation, amount > 0).
fun do_write<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    underlying: Balance<Underlying>,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    let write_amount = underlying.value();
    bucket.underlying_balance.join(underlying);

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(object::id(bucket), range_start, range_end, ctx);
    let call = coin::mint(&mut bucket.call_treasury, write_amount, ctx);
    (position, call)
}
```

### 2.3 Shared fee helper: `skim_fee`

The fee computation + treasury routing is duplicated across both flow arms today, and the RFQ
needs it too. Extract:

```move
/// Splits the protocol fee out of `premium` into the treasury; returns the
/// net premium balance. Fee = floor(premium × fee_bps / 10_000), computed
/// in u128 (matches the existing inline math exactly).
public(package) fun skim_fee<Settlement>(
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    mut premium: Balance<Settlement>,
): (Balance<Settlement>, u64 /* fee */) {
    let gross = premium.value();
    let fee = (((gross as u128) * (admin::fee_bps(config) as u128)) / 10000) as u64;
    if (fee > 0) {
        treasury::deposit_balance(treasury, premium.split(fee));
    };
    (premium, fee)
}
```

> Note: this lives in `bucket.move` (or a tiny new `fees.move`) — `treasury::deposit_balance`
> is `public(package)`, so any module in `options_protocol` can call it.

### 2.4 `execute_write_with_quote` becomes a thin wrapper

Behavior must be **byte-for-byte identical** for existing integrations (quoting-service,
frontend PTBs, indexer event decoding):

- Keep the `Writer`/`Trader` flow arms, all asserts, and all error codes exactly as they are.
- Replace the inlined fee math with `skim_fee`.
- Replace steps 8–10 with `do_write`, then `public_transfer` the returned `Position` /
  `Coin<Call>` to `position_recipient` / `call_token_recipient` exactly as today.
- `WriteExecuted` event: unchanged shape, unchanged field values. **The indexer must not
  notice this refactor happened.**

### 2.5 New event

```move
/// Emitted by write_collateralized (self-writes / venue escrow writes).
/// Deliberately distinct from WriteExecuted: it has no premium and no
/// signer — indexer treats it as a new event type, existing consumers
/// of WriteExecuted are unaffected.
public struct CollateralizedWrite has copy, drop {
    bucket_id: ID,
    writer: address,        // tx sender (the venue or self-writer)
    amount: u64,
    range_start: u128,
    range_end: u128,
}
```

(Indexer support in doc 05 §1.)

### 2.6 No new errors

All asserts reuse `bucket_expired`, `bucket_invalidated`, `zero_amount`.

## 3. Non-goals / explicitly unchanged

- `exercise`, `redeem_position`, `burn_expired_option`, `cleanup_bucket`,
  `invalidate_bucket`/`revalidate_bucket`: untouched.
- `quote.move` and the entire signed-quote verification path: untouched.
- The `Quote` BCS layout: untouched (off-chain signers keep working).
- `create_bucket` and the per-bucket OTW coin machinery: untouched.

## 4. Step-by-step

1. Add `skim_fee`, `do_write`, `write_collateralized` to `bucket.move`.
2. Rewrite the two flow arms of `execute_write_with_quote` to use `skim_fee`; replace the
   mint/cursor block with `do_write` + transfers.
3. Add `CollateralizedWrite` to `events.move` with an `emit_collateralized_write` helper
   matching the existing emit-fn style.
4. Tests (in `contracts/tests/bucket_tests.move`):
   - `write_collateralized` happy path: cursor advances, supply == outstanding, returned
     `Position` range correct, returned `Coin<Call>` value == amount.
   - Self-write round-trip: `write_collateralized` → `exercise` with the same party →
     `redeem_position` after expiry → exact conservation of underlying + settlement
     (chain-unit equality, including `apply_strike` round-half-up).
   - Expired/invalidated/zero-amount aborts.
   - **Regression**: every existing `execute_write_for_testing` test passes unchanged, and
     `WriteExecuted` event fields are identical pre/post refactor (assert on event contents,
     not just success).
5. Run `sui move test` for the whole package.

## 5. Audit notes

- `write_collateralized` is the first **public** mint path that bypasses quotes. The safety
  rests on full collateralization — flag it for the auditor with the §2.1 argument.
- The supply invariant (`coin::total_supply(call_treasury)` == unexercised, unburned option
  amount) must be re-verified to hold across all three venues (signed-quote, collateralized,
  and RFQ settle once doc 02 lands). Add a property test that interleaves all venues against
  one bucket.
