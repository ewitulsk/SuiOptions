# Covered Call Options Protocol — Design Specification

**Version**: 0.1 (MVP)
**Target chain**: Sui
**Contract language**: Sui Move
**Off-chain services**: Rust
**Transport**: WebSocket (JSON messages over WSS)

---

## 1. Overview

This protocol enables on-chain American-style covered call options on the Sui blockchain. Users in either of two roles — retail writers selling covered calls to market makers, or retail traders buying covered calls from market makers — interact with the same underlying on-chain primitives via opposite-facing RFQ (request-for-quote) channels.

The protocol's defining characteristic is its **pooled-bucket model with FIFO exercise assignment via a monotonic cursor**. All writers of the same (asset, expiry, strike) contract share a single bucket. Exercises are assigned to writers in write-order using an O(1) cursor advancement rather than per-write state mutation. Each writer's eventual economic outcome is determined entirely by their range's intersection with the cursor at expiry.

### 1.1 Roles

- **Retail Writer**: A user who wants to write a covered call against their own underlying holdings, selling it for a premium.
- **Retail Trader**: A user who wants to buy a covered call, paying a premium for the right to exercise.
- **Trader Market Maker (Trader MM)**: A professional market maker who *buys* options from retail writers. Provides bid-side liquidity.
- **Writer Market Maker (Writer MM)**: A professional market maker who *writes* options to retail traders. Provides ask-side liquidity. A single entity can play both Trader MM and Writer MM roles using one unified Account.
- **Protocol Admin**: A privileged role gated by an `AdminCap`. Performs bucket creation, fee configuration, treasury withdrawal, and dead-bucket cleanup.

### 1.2 High-level architecture

```
┌──────────────────┐         ┌──────────────────────┐         ┌──────────────────┐
│  Retail Writer   │  WSS    │                      │  WSS    │   Trader MMs     │
│    Frontend      ├────────►│                      │◄────────┤   (bots)         │
└──────────────────┘         │                      │         └──────────────────┘
                             │   Quoting Service    │
┌──────────────────┐         │       (Rust)         │         ┌──────────────────┐
│  Retail Trader   │  WSS    │                      │  WSS    │   Writer MMs     │
│    Frontend      ├────────►│                      │◄────────┤   (bots)         │
└──────────────────┘         └──────────┬───────────┘         └──────────────────┘
                                        │
                                        │ reads on-chain state
                                        ▼
                             ┌──────────────────────┐
                             │  Indexer (Rust)      │
                             └──────────┬───────────┘
                                        │
                                        ▼
                             ┌──────────────────────┐
                             │  Sui Move Contracts  │
                             │  (the Protocol)      │
                             └──────────────────────┘
```

---

## 2. Core Concepts

### 2.1 Definitions

- **Call option**: The (asset, expiry) tuple identifying a class of options.
- **Bucket**: The (asset, expiry, strike, settlement_asset) tuple. A bucket is a single shared object on Sui that pools all writes for that exact contract specification.
- **Write**: The act of depositing underlying into a bucket in exchange for a Position Object (and routing premium to the counterparty's Account).
- **Exercise**: The act of a `CallOption` holder burning their object to receive underlying from the bucket, paying `amount × strike` settlement asset in.
- **Redeem**: The post-expiry act of a Position Object holder claiming their proportional share of the bucket's underlying and settlement asset balances.

### 2.2 The cursor model

Each bucket maintains two monotonic counters:

- `total_written: u128` — sum of all underlying amounts ever written into the bucket
- `exercise_cursor: u128` — sum of all amounts ever exercised; always ≤ `total_written`

Each write occupies a contiguous half-open range `[range_start, range_end)` on the unbounded number line, where `range_start = total_written` at write time and `range_end = range_start + write_amount`. The bucket's `total_written` then advances to `range_end`.

Each exercise of amount `N` advances `exercise_cursor` by `N`. Exercises do not touch individual position state.

At redemption, a position covering `[a, b)` computes:
- `exercised_amount = max(0, min(cursor, b) - a)`
- `unexercised_amount = (b - a) - exercised_amount`
- The position holder receives `unexercised_amount` of underlying + `exercised_amount × strike` of settlement asset.

This is mathematically equivalent to FIFO assignment but requires O(1) state changes per exercise rather than O(N).

### 2.3 Property: economic alignment of premium and exercise risk

Because writes are assigned in FIFO order, early writers face exercises earlier than late writers. Early writers wrote when the option was less in-the-money (lower premium received). Late writers wrote when the option was more in-the-money (higher premium received) and sit deeper in the queue, only exercised against if the bucket faces extreme exercise pressure. Each writer's exposure to exercise corresponds to the premium they received. This is the core economic property the design preserves.

---

## 3. On-Chain Protocol (Sui Move)

### 3.1 Module structure

```
options_protocol/
├── sources/
│   ├── admin.move          // AdminCap, protocol_config
│   ├── account.move        // Account shared object, deposits, withdrawals, signing key
│   ├── bucket.move         // Bucket shared object, cursor logic, write/exercise/redeem
│   ├── call_option.move    // CallOption owned-object type, mint/burn/split/join
│   ├── position.move       // Position type, mint/burn, redemption math
│   ├── quote.move          // Quote struct, signature verification, nonce tracking
│   ├── treasury.move       // Fee treasury shared object, asset-agnostic Bag
│   ├── events.move         // All event types
│   └── errors.move         // Error code constants
└── Move.toml
```

### 3.2 Key types

#### 3.2.1 `AdminCap`

```move
/// Capability authorizing admin operations. Held by protocol operators.
public struct AdminCap has key, store {
    id: UID,
}
```

#### 3.2.2 `ProtocolConfig` (shared)

```move
public struct ProtocolConfig has key {
    id: UID,
    fee_bps: u64,                    // basis points, e.g. 50 = 0.5%; default 0
    protocol_id: vector<u8>,         // domain separator for quote signatures
}
```

#### 3.2.3 `Account` (shared)

```move
public struct Account has key {
    id: UID,
    owner: address,                  // can authorize withdrawals, key rotation
    signing_pubkey: vector<u8>,      // Ed25519 pubkey for off-chain quote signing
    // Balances stored as dynamic fields keyed by TypeName<T> for asset-agnostic storage:
    //   df::add(&mut account.id, TypeName<USDC>, Balance<USDC>)
    //   df::add(&mut account.id, TypeName<BTC>,  Balance<BTC>)
    // Nonces consumed by this account stored as dynamic fields:
    //   df::add(&mut account.id, NonceKey { nonce }, valid_until: u64)
}
```

Storing balances and consumed nonces as dynamic fields keeps `Account` generic across asset types without parameterization.

#### 3.2.4 `Bucket<Underlying, Settlement>` (shared)

```move
public struct Bucket<phantom Underlying, phantom Settlement> has key {
    id: UID,
    asset_type: TypeName,
    settlement_type: TypeName,
    expiry_ms: u64,                  // Sui clock timestamp in milliseconds
    strike: u64,                     // strike in raw settlement-asset smallest-units
    total_written: u128,
    exercise_cursor: u128,
    underlying_balance: Balance<Underlying>,
    settlement_balance: Balance<Settlement>,
}
```

`Underlying` and `Settlement` are phantom type parameters that distinguish buckets by their asset pair. The `Bucket` does not hold a `TreasuryCap` — `CallOption` (§3.2.6) is a plain Move object minted/burned by the protocol's `bucket` module directly. Bucket isolation for `CallOption` is enforced by the `bucket_id: ID` field stored on each `CallOption` and checked at exercise time. See §3.4 for the rationale and the planned Currency-standard migration.

#### 3.2.5 `Position` (owned)

```move
public struct Position has key, store {
    id: UID,
    bucket_id: ID,
    range_start: u128,
    range_end: u128,
}
```

`Position` is an owned Sui Move object — `key + store` — held in the writer's wallet. It is freely transferable via `sui::transfer::public_transfer` and can be wrapped inside other objects (kiosks, custodial vaults, DEX listings) by any holder. Burned at redemption (§3.3.6).

#### 3.2.6 `CallOption` (owned)

```move
public struct CallOption has key, store {
    id: UID,
    bucket_id: ID,
    amount: u64,
}
```

`CallOption` is an owned Sui Move object — `key + store` — held in the option buyer's wallet. Like `Position`, it is freely transferable via `sui::transfer::public_transfer` and wrappable inside other objects. It exposes `split(amount)` and `join(other)` so a holder can divide or recombine their position; the protocol enforces `bucket_id` equality on `join`. Burned at exercise (§3.3.5).

> **MVP note: `CallOption` is non-fungible at the object level.** Two `CallOption` objects with the same `bucket_id` represent equivalent rights but are distinct objects — they aren't interchangeable in a `Coin<T>` sense. The spec originally proposed `Coin<CallOptionToken<U, S>>`; that approach was dropped because Sui's Coin type requires a One-Time Witness per fungible currency, which doesn't compose with runtime bucket creation. See §3.4.

#### 3.2.7 `SignedQuote`

```move
/// The structured payload signed by the MM's hot signing key.
/// BCS-encoded for canonical bytes.
public struct Quote has copy, drop {
    protocol_id: vector<u8>,         // matches ProtocolConfig.protocol_id
    signer_account_id: ID,           // signer's Account
    signer_token_recipient: address, // address receiving signer's minted token
    bucket_id: ID,
    write_amount: u64,
    premium: u64,                    // gross, in settlement-asset smallest-units
    valid_until_ms: u64,
    nonce: u64,
}

/// Submitted to execute_write alongside the signature.
public struct SignedQuote has copy, drop {
    quote: Quote,
    signature: vector<u8>,           // Ed25519 over BCS(quote)
}
```

#### 3.2.8 `Treasury` (shared)

```move
public struct Treasury has key {
    id: UID,
    // Balances stored as dynamic fields keyed by TypeName<T>, like Account.
}
```

### 3.3 Function specifications

All functions live in their respective modules and emit events on success (see §3.5).

#### 3.3.1 Admin

```move
public fun new_call_option<Underlying, Settlement>(
    _: &AdminCap,
    expiry_ms: u64,
    start_strike: u64,
    strike_interval: u64,
    count: u64,
    coin_witness: ... ,   // see §3.4 for coin-creation pattern
    ctx: &mut TxContext,
)
```

Creates `count` buckets at strikes `start_strike`, `start_strike + strike_interval`, …, `start_strike + (count-1) * strike_interval`. For each bucket: mints the bucket as a shared object and emits `BucketCreated`. `CallOption` objects are minted on demand at `execute_write` time — no per-bucket coin currency is created.

```move
public fun set_fee_bps(_: &AdminCap, config: &mut ProtocolConfig, new_bps: u64)
public fun withdraw_treasury<T>(_: &AdminCap, treasury: &mut Treasury, amount: u64, recipient: address, ctx: &mut TxContext)
public fun cleanup_bucket<U, S>(_: &AdminCap, bucket: Bucket<U, S>, ctx: &mut TxContext)
```

`cleanup_bucket` requires: `clock.timestamp_ms() ≥ expiry_ms` AND `underlying_balance == 0` AND `settlement_balance == 0`. Destroys the bucket object.

#### 3.3.2 Account

```move
public fun create_account(signing_pubkey: vector<u8>, ctx: &mut TxContext): Account
public fun deposit<T>(account: &mut Account, coin: Coin<T>)
public fun withdraw<T>(account: &mut Account, amount: u64, ctx: &mut TxContext): Coin<T>
public fun set_quote_signing_key(account: &mut Account, new_pubkey: vector<u8>, ctx: &mut TxContext)
```

`withdraw` and `set_quote_signing_key` enforce `tx_context::sender(ctx) == account.owner`.

`create_account` returns the `Account` value; caller is expected to share it via `transfer::share_object`. Emits `AccountCreated`.

#### 3.3.3 Quote verification (internal helper)

```move
/// Verifies signature, expiry, nonce; marks nonce consumed on success.
/// Returns error otherwise. Called by execute_write.
fun verify_and_consume_quote(
    account: &mut Account,
    config: &ProtocolConfig,
    signed_quote: &SignedQuote,
    clock: &Clock,
): Quote
```

Verification steps:
1. `signed_quote.quote.protocol_id == config.protocol_id`
2. `signed_quote.quote.signer_account_id == object::id(account)`
3. `clock.timestamp_ms() < signed_quote.quote.valid_until_ms`
4. Nonce `signed_quote.quote.nonce` not already consumed for this Account
5. `ed25519_verify(&signature, &account.signing_pubkey, &bcs::to_bytes(&quote))` returns true

On success, records the nonce with its `valid_until_ms` in Account dynamic fields. Returns the verified Quote.

#### 3.3.4 Write execution (the unified entry point)

```move
public fun execute_write<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    premium_in: Coin<Settlement>,
    position_recipient: address,
    call_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

Logic (in order):

1. **Verify quote**: call `verify_and_consume_quote(signer_account, config, &signed_quote, clock)` → `quote`.
2. **Validate quote-bucket match**: `quote.bucket_id == object::id(bucket)` and `quote.write_amount == coin::value(&underlying_in) || coin::value(&premium_in)` depending on which side the signer is on (see step 3).
3. **Determine flow direction**: Inspect which of `underlying_in` or `premium_in` matches `quote.write_amount` vs `quote.premium`. The signer's side comes from `signer_account`; the executor's side comes from the passed coin.
   - **Writer flow**: signer is Trader MM; `signer_account` provides premium (debit `quote.premium` from account's Settlement balance); executor provides `underlying_in` (must equal `quote.write_amount`); `premium_in` must be empty (or refunded).
   - **Trader flow**: signer is Writer MM; `signer_account` provides underlying (debit `quote.write_amount` from account's Underlying balance); executor provides `premium_in` (must equal `quote.premium`); `underlying_in` must be empty (or refunded).

   The function distinguishes the two cases via which Coin is non-empty (or via an explicit flow-discriminator parameter — see §3.6 implementation note).

4. **Bucket-not-expired check**: `clock.timestamp_ms() < bucket.expiry_ms`.
5. **Fee skim**: `fee_amount = quote.premium * config.fee_bps / 10000`; `net_premium = quote.premium - fee_amount`. Route `fee_amount` of settlement to `treasury`.
6. **Premium routing**:
   - Writer flow: signer's Settlement balance is debited by `quote.premium`; `fee_amount` goes to treasury; `net_premium` goes to a fresh Coin sent to `tx_context::sender(ctx)` (the executor / writer).
   - Trader flow: `premium_in` is split — `fee_amount` to treasury, `net_premium` to signer's Account Settlement balance.
7. **Underlying routing**: underlying is moved into `bucket.underlying_balance`. In writer flow, from `underlying_in`. In trader flow, from a debit of signer's Underlying balance.
8. **Cursor assignment**:
   - `range_start = bucket.total_written`
   - `range_end = range_start + quote.write_amount as u128`
   - `bucket.total_written = range_end`
9. **Mint `Position`** with `(bucket_id, range_start, range_end)` → transfer to `position_recipient`.
10. **Mint `CallOption`** with `(bucket_id, amount = quote.write_amount)` → transfer to `call_token_recipient`.
11. **Emit `WriteExecuted` event**.

The function takes both `Coin<Underlying>` and `Coin<Settlement>` parameters even though only one is non-empty per flow, to keep the function signature uniform. Empty Coins are destroyed via `coin::destroy_zero`.

#### 3.3.5 Exercise

```move
public fun exercise<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    call: CallOption,
    settlement_payment: Coin<Settlement>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Underlying>
```

Logic:
1. Assert `clock.timestamp_ms() < bucket.expiry_ms`.
2. Assert `call_option::bucket_id(&call) == object::id(bucket)`.
3. `amount = call_option::amount(&call)`.
4. Assert `coin::value(&settlement_payment) == amount * bucket.strike` (in raw units).
5. Assert `bucket.exercise_cursor + (amount as u128) ≤ bucket.total_written`.
6. Burn `call` via `call_option::burn`.
7. Move `settlement_payment` into `bucket.settlement_balance`.
8. `bucket.exercise_cursor += amount as u128`.
9. Split and return `amount` of underlying from `bucket.underlying_balance`.
10. Emit `Exercised` event.

To exercise a partial amount, the holder calls `call_option::split(&mut call, amount, ctx)` first to carve off the slice they want to exercise, then passes the carved object to `exercise`.

#### 3.3.6 Redeem

```move
public fun redeem_position<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    position: Position,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Underlying>, Coin<Settlement>)
```

Logic:
1. Assert `clock.timestamp_ms() ≥ bucket.expiry_ms`.
2. Assert `position.bucket_id == object::id(bucket)`.
3. Compute:
   - `cursor = bucket.exercise_cursor`
   - `exercised = max(0, min(cursor, position.range_end) - position.range_start)`
   - `unexercised = (position.range_end - position.range_start) - exercised`
4. Withdraw `unexercised as u64` from `bucket.underlying_balance` → `Coin<Underlying>`.
5. Withdraw `(exercised as u64) * bucket.strike` from `bucket.settlement_balance` → `Coin<Settlement>`.
6. Burn the `Position` (destroy its `UID`).
7. Emit `Redeemed` event.
8. Return both coins.

#### 3.3.7 Burn expired `CallOption`

```move
public fun burn_expired_option<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    call: CallOption,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

Logic:
1. Assert `clock.timestamp_ms() ≥ bucket.expiry_ms`.
2. Assert `call_option::bucket_id(&call) == object::id(bucket)`.
3. Burn `call` via `call_option::burn`.
4. Emit `ExpiredOptionBurned` event.

### 3.4 Per-bucket token representation

**MVP design**: `CallOption` (§3.2.6) is a single non-fungible Move object type defined once in the protocol package. Every bucket mints the same struct; bucket isolation is enforced by the `bucket_id: ID` field stored on each `CallOption` and checked at `exercise` and `join`. Holders can `split` a `CallOption` to subdivide their position; the resulting child object inherits the parent's `bucket_id`. This sidesteps Sui's One-Time-Witness requirement (which would otherwise force a per-bucket package publish) at the cost of giving up native `Coin<T>` semantics — wallets see each `CallOption` as a discrete object rather than a fungible balance.

The original spec's `Coin<CallOptionToken<U, S>>` design is therefore **rejected for MVP**. Earlier drafts considered three alternatives (per-bucket published witness modules; per-(Underlying, Settlement) generic Coin types; per-bucket phantom-witness with `coin::create_currency`) and all hit the same fundamental problem: Sui's Coin standard requires module-init OTW, which cannot be invoked at runtime from a shared-object operation.

**End goal — Sui Currency standard**: Sui's [Currency standard](https://docs.sui.io/onchain-finance/fungible-tokens/currency) ships thousands of pre-deployed marker structs (`Marker0001`, `Marker0002`, …) that the protocol can register a fungible token against at runtime, without a per-token package publish. The target design is to wrap each bucket's `CallOption` rights using one of these pre-deployed markers, giving holders a true fungible `Coin`-like balance per bucket. When the supply of markers is exhausted, the protocol contract is upgraded to deploy additional markers — a routine upgrade, not a redesign.

This migration is out of MVP scope but the on-chain types are deliberately shaped so that the move is additive: `CallOption` becomes the privileged wrapper that mints/burns its corresponding Currency-marker `Coin`, leaving `bucket.move` and the cursor model untouched.

### 3.5 Events

All events live in `events.move`. Emitted via `event::emit`.

```move
public struct BucketCreated has copy, drop {
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    expiry_ms: u64,
    strike: u64,
}

public struct WriteExecuted has copy, drop {
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,             // tx_context::sender
    position_recipient: address,
    call_token_recipient: address,
    write_amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
}

public struct Exercised has copy, drop {
    bucket_id: ID,
    exerciser: address,
    amount: u64,
    settlement_paid: u64,
    cursor_after: u128,
}

public struct Redeemed has copy, drop {
    bucket_id: ID,
    position_id: ID,
    redeemer: address,
    range_start: u128,
    range_end: u128,
    underlying_returned: u64,
    settlement_returned: u64,
}

public struct ExpiredOptionBurned has copy, drop {
    bucket_id: ID,
    burner: address,
    amount: u64,
}

public struct BucketCleaned has copy, drop {
    bucket_id: ID,
}

public struct AccountCreated has copy, drop {
    account_id: ID,
    owner: address,
    signing_pubkey: vector<u8>,
}

public struct AccountDeposit has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct AccountWithdraw has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct SigningKeyRotated has copy, drop {
    account_id: ID,
    new_pubkey: vector<u8>,
}

public struct FeeUpdated has copy, drop {
    old_bps: u64,
    new_bps: u64,
}

public struct TreasuryWithdrawn has copy, drop {
    asset_type: TypeName,
    amount: u64,
    recipient: address,
}
```

### 3.6 Implementation notes

**3.6.1 Flow discriminator in `execute_write`**

Rather than inferring flow direction from which Coin is non-empty, the function may take an explicit `flow: FlowKind` enum parameter. This is clearer and prevents footguns. Decide during implementation; both are correct.

**3.6.2 Operation order is atomic**

All steps in §3.3.4 either complete together or revert. Sui PTBs guarantee transactional atomicity. No partial-state scenarios are possible at the Move level.

**3.6.3 Precision and overflow**

- All cumulative quantities use `u128` (cursor, ranges, total_written).
- Per-operation amounts use `u64` (consistent with Sui Coin values).
- Exercise math `exercised * strike` must use `u128` intermediate: cast to u128, multiply, then range-check before downcasting. Strikes denominated in stablecoin smallest-units (6 decimals for USDC) × amounts in underlying smallest-units (8 decimals for wBTC) → up to ~14 decimal digits of magnitude in intermediates. u128 (38 digits) is comfortable.
- Fee math `premium * fee_bps / 10000` also uses u128 intermediate.

**3.6.4 Nonce storage**

Each consumed nonce is stored as a dynamic field on the Account: `df::add(&mut account.id, NonceKey { nonce }, valid_until_ms)`. This grows linearly with executions; pruning is permissionless via:

```move
public fun prune_nonce(account: &mut Account, nonce: u64, clock: &Clock)
```

Anyone may call this for any account; it deletes nonces where `clock.timestamp_ms() > stored_valid_until_ms`. This shifts storage rent burden off the protocol over time.

**3.6.5 Composability**

`execute_write` is reentrancy-safe within a PTB: it operates on shared object mutable references; multiple invocations against different signers/buckets compose naturally. The same bucket can be written into and exercised against in the same PTB by different operations.

**3.6.6 Sui shared-object contention**

Buckets and Accounts are shared objects; operations on them serialize through consensus. This is expected and acceptable. Hot MM Accounts may become throughput bottlenecks; the future multi-Account-per-MM sharding pattern (mentioned in §10) is the mitigation.

### 3.7 Errors

A non-exhaustive enumeration:

| Code | Meaning |
|------|---------|
| `E_QUOTE_EXPIRED` | `valid_until_ms` has passed |
| `E_QUOTE_NONCE_USED` | Nonce already consumed for this Account |
| `E_QUOTE_SIGNATURE_INVALID` | Ed25519 verification failed |
| `E_QUOTE_PROTOCOL_MISMATCH` | `protocol_id` doesn't match |
| `E_QUOTE_BUCKET_MISMATCH` | Quote's `bucket_id` ≠ provided bucket |
| `E_QUOTE_ACCOUNT_MISMATCH` | Quote's `signer_account_id` ≠ provided account |
| `E_BUCKET_EXPIRED` | Operation requires `now < expiry` but `now ≥ expiry` |
| `E_BUCKET_NOT_EXPIRED` | Operation requires `now ≥ expiry` but `now < expiry` |
| `E_BUCKET_NOT_DRAINED` | `cleanup_bucket` called on bucket with remaining balances |
| `E_INSUFFICIENT_ACCOUNT_BALANCE` | Account lacks balance for quote |
| `E_AMOUNT_MISMATCH` | Provided Coin value ≠ quote.write_amount or quote.premium |
| `E_SETTLEMENT_AMOUNT_MISMATCH` | Exercise payment ≠ `amount × strike` |
| `E_CURSOR_OVERFLOW` | Exercise would advance cursor past `total_written` |
| `E_NOT_OWNER` | Caller is not Account owner |
| `E_POSITION_BUCKET_MISMATCH` | `Position`'s `bucket_id` ≠ provided bucket |

---

## 4. Quote Format (Off-Chain ↔ On-Chain Contract)

This is the canonical structure exchanged between the Quoting Service, MMs, and the protocol.

### 4.1 Canonical bytes

The signed payload is the BCS encoding of the `Quote` struct (§3.2.7). Field order:

```
protocol_id:            vector<u8>
signer_account_id:      ID (32 bytes)
signer_token_recipient: address (32 bytes)
bucket_id:              ID (32 bytes)
write_amount:           u64 (little-endian)
premium:                u64 (little-endian)
valid_until_ms:         u64 (little-endian)
nonce:                  u64 (little-endian)
```

### 4.2 Signature

Ed25519 over the BCS bytes. Signed by the Account's registered `signing_pubkey`. Verified on-chain via `sui::ed25519::ed25519_verify`.

### 4.3 Wire format

When transmitted over WebSocket (between Quoting Service and clients) the quote is JSON-encoded:

```json
{
  "quote": {
    "protocol_id": "0x...",
    "signer_account_id": "0x...",
    "signer_token_recipient": "0x...",
    "bucket_id": "0x...",
    "write_amount": "10000000",
    "premium": "50000000",
    "valid_until_ms": "1748534400000",
    "nonce": "42"
  },
  "signature": "0x..."
}
```

Numeric fields (write_amount, premium, valid_until_ms, nonce) are serialized as decimal strings to avoid JS precision loss. The Quoting Service re-encodes via BCS for on-chain submission and signature verification.

### 4.4 TTL

Default `valid_until_ms - now` should be in the range 30–60 seconds at signing time. MMs may sign shorter TTLs for tighter spreads.

---

## 5. Quoting Service (Rust)

### 5.1 Responsibilities

The Quoting Service is a stateful Rust application that:

1. Accepts WebSocket connections from retail frontends (writer-side and trader-side) and from MM bots (writer-MM and trader-MM, possibly the same MM serving both roles).
2. Maintains an in-memory model of:
   - Each Account's on-chain balances (per asset type), refreshed from the indexer.
   - Each Account's outstanding quote reservations (per asset type).
   - Effective `available_balance = on_chain_balance - active_reservations` per (Account, asset_type).
   - MM reputation scores per Account.
3. Routes RFQs from retail to MMs: when a retail user requests a quote for a bucket, the service broadcasts the request to all subscribed MMs on the relevant side and aggregates responses.
4. Validates incoming MM quotes: signature, expiry, and reservation feasibility against `available_balance`. Rejects quotes that would oversubscribe an MM.
5. Marks reservations: on accepted quotes, decrements `available_balance`; on TTL expiry or execution confirmation from the indexer, releases the reservation.
6. Forwards validated quotes to the requesting retail client.
7. Updates reputation scores from indexer-observed execution outcomes.

The Quoting Service holds no funds and signs no transactions. It is a routing and bookkeeping layer.

### 5.2 Service architecture

```
quoting-service/
├── src/
│   ├── main.rs              // entry point, config, spawn tasks
│   ├── ws/
│   │   ├── mod.rs           // WebSocket server
│   │   ├── retail.rs        // retail client connection handler
│   │   ├── mm.rs            // MM connection handler
│   │   ├── messages.rs      // serde types for all WS messages
│   │   └── auth.rs          // session establishment, signing-key challenge
│   ├── state/
│   │   ├── mod.rs
│   │   ├── accounts.rs      // AccountState: balances, reservations
│   │   ├── reservations.rs  // active reservation table, TTL eviction
│   │   ├── reputation.rs    // MM rep tracking
│   │   └── buckets.rs       // known buckets (mirror of indexer)
│   ├── rfq/
│   │   ├── mod.rs           // RFQ orchestration
│   │   ├── writer_flow.rs   // routes writer-side RFQs to trader MMs
│   │   ├── trader_flow.rs   // routes trader-side RFQs to writer MMs
│   │   └── matcher.rs       // collects responses, applies time windows
│   ├── indexer_client.rs    // subscribes to indexer for chain events
│   ├── chain.rs             // Sui RPC client for occasional direct reads
│   ├── config.rs
│   └── errors.rs
├── Cargo.toml
└── README.md
```

### 5.3 Key crates

- `tokio` — async runtime
- `tokio-tungstenite` — WebSocket
- `serde`, `serde_json` — message serialization
- `dashmap` — concurrent hashmaps for state
- `bcs` — for verifying / re-encoding signed quotes
- `ed25519-dalek` — signature verification (defense-in-depth alongside on-chain)
- `sui-sdk` — for occasional direct RPC reads
- `tracing` — structured logging
- `metrics` — Prometheus metrics

### 5.4 WebSocket protocol

#### 5.4.1 Connection lifecycle

All connections are WSS (TLS). On connect, the client sends a `Hello` message identifying their role. MM connections additionally authenticate by signing a server-issued challenge with their Account's signing key (proving they control the Account).

Heartbeat: server sends `Ping` every 15 seconds; client must respond `Pong` within 5 seconds or be disconnected.

#### 5.4.2 Message envelope

Every message is a JSON object with a `type` discriminator and a payload:

```json
{ "type": "RFQRequest", "request_id": "...", "payload": { ... } }
```

`request_id` is a client-generated unique string for correlating requests and responses.

#### 5.4.3 Retail → Service messages

**`Hello`** — declares role (`"writer"` or `"trader"`) and frontend version.

```json
{ "type": "Hello", "payload": { "role": "writer", "version": "1.0.0" } }
```

**`SubscribeBuckets`** — request a stream of bucket state updates (cursor, total_written, expiry) for given bucket IDs. Used to power the queue-position display.

```json
{ "type": "SubscribeBuckets", "payload": { "bucket_ids": ["0x...", "0x..."] } }
```

**`RFQRequest`** — request quotes for a specific bucket and write amount.

```json
{
  "type": "RFQRequest",
  "request_id": "req-abc",
  "payload": {
    "bucket_id": "0x...",
    "write_amount": "10000000",
    "side": "writer"   // "writer" = retail wants to write; "trader" = retail wants to buy
  }
}
```

The service broadcasts to MMs of the opposite side, collects responses for a configurable window (default 2 seconds), and returns aggregated quotes to the retail client.

#### 5.4.4 Service → Retail messages

**`HelloAck`** — acknowledges connection.

**`BucketUpdate`** — pushed on subscribed bucket state changes.

```json
{
  "type": "BucketUpdate",
  "payload": {
    "bucket_id": "0x...",
    "total_written": "150000000",
    "exercise_cursor": "30000000",
    "expiry_ms": "1748534400000"
  }
}
```

**`RFQResponse`** — quotes returned for an `RFQRequest`.

```json
{
  "type": "RFQResponse",
  "request_id": "req-abc",
  "payload": {
    "bucket_id": "0x...",
    "write_amount": "10000000",
    "quotes": [
      {
        "quote": { /* full Quote object */ },
        "signature": "0x...",
        "mm_id": "0xmm...",
        "mm_reputation": 0.97
      },
      ...
    ]
  }
}
```

Returned in order of best price for the retail user (highest premium first for writer-side, lowest premium first for trader-side).

#### 5.4.5 MM → Service messages

**`Hello`** — declares role (`"trader_mm"` and/or `"writer_mm"`), Account ID, and signing pubkey.

**`AuthResponse`** — response to the service's challenge.

```json
{ "type": "AuthResponse", "payload": { "signature": "0x..." } }
```

**`Quote`** — response to an RFQ broadcast from the service.

```json
{
  "type": "Quote",
  "request_id": "req-abc",
  "payload": {
    "quote": { /* Quote object */ },
    "signature": "0x..."
  }
}
```

MMs may also send unsolicited `Quote` messages bound to a `subscription_id` if they're streaming standing quotes (out of MVP scope, but the protocol allows it).

**`Decline`** — explicit decline of an RFQ.

```json
{ "type": "Decline", "request_id": "req-abc", "payload": { "reason": "..." } }
```

#### 5.4.6 Service → MM messages

**`AuthChallenge`** — random bytes to be signed.

**`AuthAck`** — confirms successful auth.

**`RFQBroadcast`** — relayed RFQ from a retail user.

```json
{
  "type": "RFQBroadcast",
  "request_id": "req-abc",
  "payload": {
    "bucket_id": "0x...",
    "write_amount": "10000000",
    "side": "writer",
    "deadline_ms": "1748534400000"   // service-imposed response deadline
  }
}
```

**`AccountStateUpdate`** — pushed when the MM's on-chain Account balance changes (from indexer).

```json
{
  "type": "AccountStateUpdate",
  "payload": {
    "account_id": "0x...",
    "balances": { "USDC": "1000000000", "BTC": "50000000" },
    "active_reservations": { "USDC": "200000000" },
    "available": { "USDC": "800000000", "BTC": "50000000" }
  }
}
```

**`ReservationConfirmed`** / **`ReservationReleased`** — signals when a quote the MM signed has had a reservation confirmed (signed and tracked) or released (TTL or execution).

### 5.5 Reservation logic

Reservations are keyed by `(account_id, nonce)` and store:

```rust
struct Reservation {
    account_id: ObjectId,
    nonce: u64,
    asset_type: TypeName,
    amount: u64,
    valid_until_ms: u64,
    created_at_ms: u64,
}
```

A background tokio task evicts reservations where `now > valid_until_ms`. On eviction, the corresponding asset amount is returned to `available_balance` for that Account.

When the indexer reports a `WriteExecuted` event mentioning a `(signer_account_id, nonce)` matching an active reservation, the reservation is released — but the `available_balance` is *not* incremented on release (the underlying balance has already been debited on-chain, so the indexer's balance update reflects the new state).

### 5.6 Reputation

Per Account, track:

```rust
struct ReputationStats {
    quotes_signed: u64,
    quotes_executed: u64,
    quotes_expired_unused: u64,
    quotes_reverted: u64,
    median_response_latency_ms: u64,
    revert_rate: f64,           // reverted / signed
    fill_rate: f64,             // executed / signed
}
```

Updated from indexer events. Used to:
- Filter quotes shown to retail (e.g., drop MMs with revert_rate > 5%).
- Sort competing quotes by composite score (price, then reputation).
- Eventually rate-limit or ban consistently bad MMs.

For MVP with self-operated MMs, reputation is observed-only and doesn't gate behavior.

### 5.7 Indexer interaction

The Quoting Service consumes events from the Indexer (§6) via a separate WebSocket or gRPC channel. The events it cares about:

- `BucketCreated` → add to known buckets
- `WriteExecuted` → update Account balances; release matching reservation
- `Exercised` → update bucket cursor (push to subscribers)
- `AccountDeposit` / `AccountWithdraw` → update Account balance
- `SigningKeyRotated` → update Account's known pubkey

The Quoting Service does NOT read state directly from the chain on the hot path; it relies on the Indexer's stream for consistency. Direct RPC reads are used only for cold-start initialization or recovery from indexer lag.

### 5.8 RFQ orchestration flow (detailed)

When a retail writer sends `RFQRequest`:

1. Service validates `bucket_id` exists and is not expired.
2. Service generates `RFQBroadcast` with a `deadline_ms = now + 2000ms`.
3. Service identifies all currently-connected Trader MMs (writer-flow MMs are trader-side counterparties).
4. Service broadcasts to those MMs.
5. Service collects `Quote` responses until `deadline_ms`.
6. For each received `Quote`:
   - Validate signature against the MM's known pubkey.
   - Validate `quote.bucket_id`, `quote.write_amount`, `quote.valid_until_ms`.
   - Check `available_balance[USDC] ≥ quote.premium` for the MM's Account.
   - If valid: record reservation; mark quote as eligible.
   - If invalid: drop, log reason, increment MM's invalid-quote counter.
7. Sort eligible quotes (highest premium first for writer flow).
8. Send `RFQResponse` to the retail user.
9. Notify each quoting MM whether their reservation was confirmed.

The trader flow is symmetric, broadcasting to Writer MMs and sorting by lowest premium.

### 5.9 Failure modes

- **MM disconnect mid-RFQ**: their quote is dropped; reservation never created.
- **Indexer lag**: the service operates on possibly-stale balance data, leading to occasional optimistic reservations that fail on-chain. The on-chain revert is the safety net.
- **MM signs quote then withdraws**: detected by indexer; reservation released; if writer attempts execution, on-chain revert (insufficient balance). Reputation damaged.
- **Network partition between service and indexer**: service should refuse new RFQs after a configurable staleness threshold (e.g., 5 seconds of no indexer events).

---

## 6. Indexer (Rust)

### 6.1 Responsibilities

A separate Rust service that:

1. Subscribes to Sui's event stream for the protocol's package.
2. Persists every emitted event to a database (Postgres recommended).
3. Maintains derived views: per-Account balances, per-bucket cursor state, per-position status.
4. Exposes a WebSocket or gRPC stream for the Quoting Service and any frontends to subscribe to event streams.
5. Provides read APIs for historical queries (e.g., "show me all writes by Account X over the last week").

The Indexer is read-only with respect to the chain.

### 6.2 Architecture

Standard Sui indexer pattern: tail the event log via Sui's `suix_subscribeEvent` or batch-poll `suix_queryEvents`, materialize into Postgres, expose via WebSocket fanout.

Recommended schema:

- `bucket_events` (BucketCreated, BucketCleaned)
- `write_events` (WriteExecuted)
- `exercise_events` (Exercised)
- `redeem_events` (Redeemed)
- `option_burn_events` (ExpiredOptionBurned)
- `account_events` (AccountCreated, AccountDeposit, AccountWithdraw, SigningKeyRotated)
- `treasury_events` (TreasuryWithdrawn, FeeUpdated)
- `accounts` materialized view (current balances per Account per asset)
- `buckets` materialized view (current cursor, total_written, expiry)
- `positions` materialized view (active `Position` object IDs by Account)

### 6.3 Why a separate service from the Quoting Service

- Indexing is CPU- and storage-intensive; quoting is latency-sensitive. Separating allows independent scaling.
- Indexer may serve multiple downstream consumers (frontends, analytics, monitoring, future research).
- Failure isolation: indexer downtime degrades but doesn't kill the Quoting Service (which can fall back to direct RPC for critical reads).

---

## 7. Frontend Interactions

### 7.1 Retail Writer Frontend

1. User connects wallet; frontend discovers user's EOA.
2. User selects bucket (asset, expiry, strike) via a browse UI populated from the Indexer.
3. Frontend opens WebSocket to Quoting Service, sends `Hello { role: "writer" }`.
4. Frontend subscribes to bucket updates via `SubscribeBuckets`.
5. Frontend displays current bucket state, including projected queue position (= current `total_written`).
6. User specifies `write_amount`; frontend sends `RFQRequest`.
7. Frontend receives `RFQResponse` with multiple quotes; displays them sorted by premium.
8. User selects a quote.
9. Frontend constructs a PTB:
   - (If using an Account) `account::withdraw<Underlying>(amount)` → Coin
   - (Else) splits user's wallet Coin to exact amount
   - Wraps the `SignedQuote` from the chosen quote
   - Calls `execute_write(bucket, config, treasury, mm_account, underlying_coin, zero_settlement_coin, writer_addr, mm_token_recipient, signed_quote, clock)`
10. User signs and submits PTB.
11. On confirmation, frontend shows: net premium received, Position Object received.

### 7.2 Retail Trader Frontend

Mirror of writer flow, with `role: "trader"` and the PTB calling `execute_write` with premium as the executor's Coin and underlying as the signer's Account-debit.

### 7.3 MM Bot Interface

MMs run their own bots that connect to the Quoting Service as `trader_mm`, `writer_mm`, or both. The bot:

1. Maintains its own pricing model (vol surface, hedging book, etc.).
2. Responds to `RFQBroadcast` messages within the response window.
3. Listens to `AccountStateUpdate` to track available balance.
4. Listens to chain events (via its own indexer subscription) to track its written positions and held call options.

The MM bot is not part of this protocol's deliverables; this spec defines only the interface.

---

## 8. Sequence Diagrams

### 8.1 Writer flow (retail writer ← trader MM)

```
RetailWriter   Frontend     QuotingService   TraderMM      Chain
     │            │              │               │           │
     │ Open WS    │              │               │           │
     │───────────►│ Hello        │               │           │
     │            │─────────────►│               │           │
     │            │ ◄─HelloAck───│               │           │
     │ Pick bucket│              │               │           │
     │ + amount   │ RFQRequest   │               │           │
     │───────────►│─────────────►│ RFQBroadcast  │           │
     │            │              │──────────────►│           │
     │            │              │               │ price it  │
     │            │              │ ◄────Quote────│           │
     │            │              │ validate, reserve         │
     │            │ ◄─RFQResponse│ ReservationConfirmed→     │
     │ Pick quote │              │───────────────►           │
     │───────────►│              │               │           │
     │            │ Build PTB    │               │           │
     │ Sign PTB   │              │               │           │
     │───────────►│              │               │           │
     │            │──────────────────────────────────────────►│
     │            │              │               │  verify   │
     │            │              │               │  skim fee │
     │            │              │               │  mint Object │
     │            │              │               │  mint Tok │
     │            │              │               │  emit ev  │
     │            │ ◄────────────────────────────────────────│
     │ ◄─Done─────│              │               │           │
     │            │              │ ←──Event(WriteExecuted)──│
     │            │              │ release reservation       │
     │            │              │ AccountStateUpdate→       │
     │            │              │───────────────►           │
```

### 8.2 Trader flow (retail trader ← writer MM)

Symmetric. The retail trader executes; the writer MM signs and provides underlying from their Account.

### 8.3 Exercise

```
Holder        Frontend       Chain
  │              │              │
  │ Want exercise│              │
  │─────────────►│ Build PTB    │
  │              │  - account::withdraw<Settlement>(amount * strike)
  │              │  - exercise(bucket, call_option, settlement_payment, clock)
  │ Sign PTB     │              │
  │─────────────►│             │
  │              │─────────────►│
  │              │              │ check not expired
  │              │              │ check cursor + amount ≤ total_written
  │              │              │ burn tokens
  │              │              │ move settlement in
  │              │              │ advance cursor
  │              │              │ split underlying out
  │              │              │ emit Exercised
  │              │ ◄────────────│
  │ ◄─Done───────│              │
```

### 8.4 Redeem

```
Writer        Frontend       Chain
  │              │              │
  │ After expiry │              │
  │─────────────►│ Build PTB    │
  │              │  - redeem_position(bucket, position, clock)
  │ Sign PTB     │              │
  │─────────────►│              │
  │              │─────────────►│
  │              │              │ check expired
  │              │              │ compute exercised/unexercised
  │              │              │ split out underlying + settlement
  │              │              │ burn Object
  │              │              │ emit Redeemed
  │              │ ◄────────────│
  │ ◄─Done───────│              │
```

---

## 9. Security and Threat Model

### 9.1 Trust assumptions

- **Protocol Admin**: Trusted to create sensible buckets, set reasonable fees, hold the admin key securely. Compromise of AdminCap is catastrophic for new bucket creation and fee setting but cannot directly drain user funds.
- **Quoting Service**: Trusted to route quotes faithfully. Compromise could enable: censorship of MMs/retail users, biased ordering of quotes, leaking RFQs to specific MMs first (front-running RFQs). Cannot drain funds.
- **Indexer**: Trusted to report chain state accurately. Compromise could enable spoofed state to the Quoting Service. Mitigated by Quoting Service spot-checking via direct RPC.
- **MMs**: Untrusted. Protocol assumes adversarial MMs may oversign quotes, withdraw at inopportune times, or grief retail users via reverts. Mitigations: on-chain balance checks, reputation tracking, short TTLs.
- **Retail users**: Untrusted but only able to harm themselves.

### 9.2 Attack scenarios

**Scenario 1: MM oversubscription attack**. MM signs 100x more quotes than they can back. Retail users execute, some succeed, some revert. **Mitigation**: Quoting Service tracks reservations; surfaces revert rate; eventually filters bad MMs.

**Scenario 2: MM withdraws right before execution**. MM signs quote, retail starts executing, MM withdraws funds, retail's TX reverts. **Mitigation**: reputation system disincentivizes; accepted MVP behavior.

**Scenario 3: Quote replay**. Attacker captures a signed quote and replays. **Mitigation**: nonce uniqueness per Account, enforced on-chain.

**Scenario 4: Frontrunning RFQ**. Quoting Service operator leaks RFQs to favored MMs. **Mitigation**: out of scope for MVP. Could be addressed by signed RFQs, decentralized RFQ relay, or batching.

**Scenario 5: Bucket cursor manipulation**. None possible — cursor only advances via legitimate exercises, which require burning a `CallOption` and paying full settlement.

**Scenario 6: Reentrancy**. Move's resource model precludes reentrancy attacks within a single PTB; objects are linearly typed.

**Scenario 7: Forgery of signed quote**. Requires breaking Ed25519. Out of scope.

**Scenario 8: Admin key compromise**. Admin can create bogus buckets, change fees, withdraw treasury. Cannot touch user funds in buckets or accounts. **Mitigation**: AdminCap held in multisig (off-protocol, operationally).

### 9.3 Audited surfaces

The following are critical and warrant audit attention:

1. Cursor math in `execute_write` and `redeem_position`.
2. Signature verification and nonce tracking in `verify_and_consume_quote`.
3. Fee skim arithmetic and rounding behavior.
4. `CallOption` bucket-isolation: the `bucket_id` field on each `CallOption` is the only thing preventing a holder from exercising one bucket's `CallOption` against another bucket. Verify the `exercise` and `call_option::join` `bucket_id` checks are airtight.
5. Sui-specific concerns: shared object access patterns, dynamic field key collisions, clock object usage.

---

## 10. Out of Scope / Future Work

- Multi-key Accounts (cold key + hot key separation).
- Multi-MM aggregated quotes (writer requests size, service splits across MMs, single PTB executes multiple `execute_write` calls).
- Exchange integration / order book listing of `CallOption` objects (likely gated on the Currency-standard migration described in §3.4).
- Reversed-flow frontend (retail trader buying calls) — protocol supports it, frontend is future work.
- Decentralization of the Quoting Service.
- Cross-chain bridging for underlying assets.
- More sophisticated MM rate-limiting / staking-for-reputation.
- Stale-position sweep mechanism.
- Account contention sharding (multiple Accounts per MM).

---

## 11. Implementation Roadmap (Suggested)

**Phase 1 — Core protocol**
- Bucket, Account, Position, CallOption types
- `execute_write`, `exercise`, `redeem_position`, `burn_expired_option`
- Quote signature verification, nonce tracking
- Event emission

**Phase 2 — Admin and treasury**
- AdminCap, ProtocolConfig
- `new_call_option`, `set_fee_bps`, `withdraw_treasury`, `cleanup_bucket`
- Fee skim in `execute_write`

**Phase 3 — Indexer**
- Event subscription
- Postgres schema, materialized views
- WebSocket fanout

**Phase 4 — Quoting Service**
- WebSocket server, retail and MM handlers
- Reservation table and TTL eviction
- RFQ orchestration
- Reputation tracking

**Phase 5 — Frontend**
- Writer-side frontend
- Bucket browser, RFQ flow, PTB construction
- Position management, redemption UI

**Phase 6 — Reverse flow**
- Trader-side frontend
- Writer-MM bot reference implementation

---

*End of specification — v0.1*
