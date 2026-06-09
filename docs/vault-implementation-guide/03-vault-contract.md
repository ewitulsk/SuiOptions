# 03 — The Vault Contract (`vault.move`)

**Files**: new `contracts/sources/vault.move`, new `contracts/sources/oracle.move`,
additions to `events.move`/`errors.move`, Pyth dependency in `contracts/Move.toml`
**Depends on**: 01 (write core), 02 (RFQ)
**Blocks**: 04 (keeper), 05 (indexer/api), frontend

## 1. Principles

1. **Permissionless lifecycle.** Every round-lifecycle function is a public crank with full
   on-chain validation. The keeper has zero privileges; a malicious cranker can at worst
   waste their own gas. Admin (`AdminCap`) only: creates the vault, sets config parameters
   (fees, guardrail bounds, slice limits), and can pause new deposits. Admin can **not**
   move user funds or skip the state machine.
2. **The state machine is the spec.** Rounds advance through explicit phases; each crank is
   legal in exactly one phase; phase transitions are triggered by time + completed work, not
   by trust.
3. **Oracle-bounded discretion.** Wherever a crank has a degree of freedom (which bucket,
   what reserve premium, what swap min-out), Pyth bounds it so the freedom can't be abused.

## 2. New on-chain dependency: Pyth

Add to `contracts/Move.toml`:

```toml
Pyth = { git = "https://github.com/pyth-network/pyth-crosschain.git", subdir = "target_chains/sui/contracts", rev = "<pin>" }
```

New module `oracle.move` wraps it:

```move
module options_protocol::oracle;

/// Returns the U/S cross price in "settlement smallest-units per underlying
/// smallest-unit", as (price_scaled: u128, scale: u8) — same shape as the
/// bucket's strike encoding, so strike/spot comparisons are exact integer
/// math at a common scale.
public fun spot_cross(
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    expected_underlying_feed: vector<u8>,   // from VaultConfig
    expected_settlement_feed: vector<u8>,
    underlying_decimals: u8,
    settlement_decimals: u8,
    max_age_secs: u64,
    clock: &Clock,
): (u128, u8)
```

Checks: feed IDs match the expected ones (the caller cannot substitute a different market),
publish time within `max_age_secs`, price positive, confidence within a configured ratio.
This mirrors what `mm-bot/src/pricing.rs::compute_spot_from_prices` does off-chain
(cross = underlying_usd / settlement_usd, rescaled by decimal difference) — keep the two
implementations test-locked against shared vectors.

## 3. Share token

Each vault gets a **fungible share coin** (`Coin<VShare>`), minted/burned solely by the vault
via a `TreasuryCap<VShare>` it owns — the exact pattern buckets already use for `Coin<Call>`.
Reuse the option-scheduler's coin machinery (`option-scheduler/src/codegen.rs` +
`sui-tx/src/tx/coin_pkg.rs::publish_coin_package`) to publish one OTW coin module per vault
at deployment time (vault count is tiny: 2 at launch).

Why a coin and not internal accounting: transferable/composable shares (DeepBook listing,
collateral use, future staking), wallet-visible balances, and the receipt flow below stays
simple. Share coin decimals = underlying decimals.

## 4. Types

```move
module options_protocol::vault;

public struct Vault<phantom U, phantom S, phantom VShare> has key {
    id: UID,
    config: VaultConfig,

    // ---- round state machine ----
    round: u64,
    phase: Phase,
    current_bucket: Option<ID>,
    current_expiry_ms: u64,            // expiry of current_bucket; 0 when none
    selling_ends_ms: u64,              // open_rfq forbidden after this

    // ---- share accounting ----
    share_treasury: TreasuryCap<VShare>,
    /// Price per share in PPS_SCALE fixed point, frozen per round at
    /// finalize. pps[r] is the price at which round-r receipts convert.
    pps: Table<u64, u128>,

    // ---- balances ----
    deployable: Balance<U>,            // assets working in the current round
    pending_deposits: Balance<U>,      // queued; join deployable at next finalize
    proceeds_settlement: Balance<S>,   // premium + exercise proceeds awaiting swap/finalize
    withdrawal_pool: Balance<U>,       // fully reserved for completed WithdrawReceipts
    queued_withdraw_shares: Balance<VShare>, // shares escrowed by initiate_withdraw

    // ---- per-round working state ----
    positions: ObjectTable<u64, Position>,  // FIFO of positions awaiting redeem
    positions_head: u64,
    positions_tail: u64,
    open_rfqs: u64,                    // RFQs created and not yet settled/absorbed
    round_premium_collected: u64,      // settlement units, gross of vault fees, net of protocol fee
    round_deposits_pending: u64,       // mirror of pending_deposits.value() at finalize time

    paused_deposits: bool,
}

public enum Phase has copy, drop, store {
    /// Bucket expired; redeeming positions and swapping proceeds.
    Settling,
    /// Round live: bucket selected, RFQs may be opened/settled until selling_ends_ms.
    Active,
}

public struct VaultConfig has copy, drop, store {
    // fees (charged only on profitable rounds; doc §8)
    mgmt_fee_bps_annual: u64,          // e.g. 200  = 2%/yr
    perf_fee_bps: u64,                 // e.g. 1000 = 10% of premium

    // round shape
    round_ms: u64,                     // 7 days
    selling_window_ms: u64,            // e.g. 12h after round start

    // bucket-selection guardrails (vs Pyth spot)
    min_strike_bps_over_spot: u64,     // e.g. 300  ⇒ strike ≥ 1.03 × spot (never sell ITM/ATM)
    max_strike_bps_over_spot: u64,     // e.g. 6000 ⇒ strike ≤ 1.60 × spot (premium must be meaningful)
    min_expiry_lead_ms: u64,           // bucket expiry ≥ now + lead (e.g. 3 days)
    max_expiry_lead_ms: u64,           // bucket expiry ≤ now + cap  (e.g. 9 days)

    // RFQ slice guardrails
    min_reserve_premium_bps: u64,      // reserve ≥ bps × spot-notional (e.g. 10 bps)
    max_slice_amount: u64,             // underlying units per RFQ
    max_open_rfqs: u64,
    rfq_duration_ms: u64,
    rfq_snipe_window_ms: u64,
    rfq_snipe_extension_ms: u64,
    rfq_max_extension_ms: u64,
    rfq_min_increment_bps: u64,

    // proceeds / swap policy
    hold_premium_in_settlement: bool,  // backtest-decided (doc 06); false ⇒ swap everything to U
    max_swap_slippage_bps: u64,        // min_out = pyth_value × (1 − slippage)

    // oracle
    underlying_feed_id: vector<u8>,
    settlement_feed_id: vector<u8>,
    max_price_age_secs: u64,
    underlying_decimals: u8,
    settlement_decimals: u8,
}

const PPS_SCALE: u128 = 1_000_000_000_000; // 1e12

public struct DepositReceipt has key, store {
    id: UID,
    vault_id: ID,
    round: u64,        // shares claimable once pps[round] exists
    amount: u64,       // underlying units deposited
}

public struct WithdrawReceipt has key, store {
    id: UID,
    vault_id: ID,
    round: u64,        // payable once pps[round] exists (settles at pps[round])
    shares: u64,
}
```

Position storage is a FIFO (`ObjectTable<u64, Position>` + head/tail indices) so settling can
be cranked one position per call with bounded gas, no iteration.

## 5. User functions

### 5.1 `deposit`

```move
public fun deposit<U, S, V>(vault: &mut Vault<U, S, V>, coin: Coin<U>, ctx): DepositReceipt
```

- `!paused_deposits`; `coin.value() > 0`.
- Joins `pending_deposits`; receipt's `round = vault.round + 1` (the deposit participates
  from the *next* round — never exposed to the current round's P&L).
- Emits `VaultDeposit`.

### 5.2 `claim_shares`

```move
public fun claim_shares<U, S, V>(vault: &mut Vault<U, S, V>, receipt: DepositReceipt, ctx): Coin<V>
```

- Requires `pps` to contain `receipt.round − 1` (set when the receipt's round started —
  see the receipt round convention in §7.4).
- `shares = amount × PPS_SCALE / pps[receipt.round − 1]` (u128 math, floor; dust favors
  the vault).
- Mints from `share_treasury`. Burns the receipt. Emits `SharesClaimed`.

### 5.3 `initiate_withdraw`

```move
public fun initiate_withdraw<U, S, V>(vault: &mut Vault<U, S, V>, shares: Coin<V>, ctx): WithdrawReceipt
```

- Escrows the share coin into `queued_withdraw_shares`; receipt `round = vault.round`
  (the withdrawer **is** exposed to the current round — Ribbon semantics).
- Emits `WithdrawInitiated`.

### 5.4 `complete_withdraw`

```move
public fun complete_withdraw<U, S, V>(vault: &mut Vault<U, S, V>, receipt: WithdrawReceipt, ctx): Coin<U>
```

- Requires `pps[receipt.round]` to exist (round finalized).
- `amount = shares × pps[receipt.round] / PPS_SCALE` (floor), paid from `withdrawal_pool`
  (which finalize fully funded — §7.4). Emits `WithdrawCompleted`.

### 5.5 `instant_withdraw_pending`

Cancels a not-yet-active deposit: burns a `DepositReceipt` whose `round > vault.round`
(i.e. its round hasn't started) and returns the amount from `pending_deposits`. Mirrors
Ribbon's "instant withdrawal" for queued funds.

## 6. Reserve premium & strike bounds (the Pyth guardrails)

With a permissionless keeper, the two value-leak vectors are: selecting a garbage bucket, and
auctioning calls with no price floor. Both are bounded by Pyth at crank time:

- **Strike bound** (at `select_bucket`): with `(spot, scale_s)` from `oracle::spot_cross`
  and the bucket's `(strike, strike_scale)`, require (cross-multiplied integer compare —
  no floats on-chain):

  ```
  strike / 10^strike_scale ≥ spot / 10^scale_s × (1 + min_strike_bps/10⁴)
  strike / 10^strike_scale ≤ spot / 10^scale_s × (1 + max_strike_bps/10⁴)
  ```

- **Reserve floor** (at `open_rfq`):

  ```
  spot_notional = apply_strike-style mul: amount × spot / 10^scale_s          (u128, round-half-up)
  reserve_premium = max(min_reserve_premium_bps × spot_notional / 10⁴, 1)
  ```

  The reserve is intentionally a *floor*, not a fair price — competition discovers price;
  the floor only prevents a quiet auction from giving calls away. 10 bps of notional is far
  below any plausible weekly 0.1-delta premium (~15–60 bps), so it never blocks honest fills.

The off-chain keeper computes the *target* strike (0.1-delta, doc 04 §3) and picks the bucket;
the on-chain bounds make a hostile keeper's worst case "slightly suboptimal strike inside the
band", not theft.

## 7. Lifecycle cranks (all `public`, all permissionless)

```
            finalize_round                    select_bucket          selling_ends_ms
 Settling ────────────────────► Active ────────(sets bucket)──► … RFQs … ──────────► (hold to expiry)
    ▲                                                                                      │
    │                              expiry reached: crank_redeem × N, swap_proceeds          │
    └──────────────────────────────────────────────────────────────────────────────────────┘
```

A round = one trip around this loop. Genesis: vault starts in `Settling` with no bucket and
`pps[0] = PPS_SCALE`; the first `finalize_round` activates round 1 with the initial deposits.

### 7.1 `crank_redeem`

```move
public fun crank_redeem<U, S, V, C>(vault, bucket: &mut Bucket<U,S,C>, clock, ctx)
```

- Legal when `phase == Active` and `now ≥ current_expiry_ms` **or** already `Settling`
  (first call flips the phase to `Settling`).
- Pops `positions[positions_head]`, requires its `bucket_id == current_bucket`, calls
  `bucket::redeem_position`; underlying → `deployable`, settlement → `proceeds_settlement`;
  head++. One position per call ⇒ bounded gas. Emits `VaultPositionRedeemed`.

### 7.2 Unsettled-RFQ recovery

`open_rfqs` must be 0 before finalize. RFQs always resolve via `rfq::settle` (doc 03 §7.5's
`settle_rfq` decrements the counter) or `rfq::settle_expired` → `vault::absorb_refund`
(returns the collateral `Coin<U>` to `deployable`, decrements). Both are permissionless, so
no admin is ever needed to unstick a round.

### 7.3 `swap_proceeds` (DeepBook — interface stub)

> The DeepBook integration is being built in parallel; api-service will expose the pool
> address per pair. The vault isolates the venue behind one function so the adapter can land
> independently.

```move
public fun swap_proceeds<U, S, V>(
    vault: &mut Vault<U, S, V>,
    pool: &mut Pool<U, S>,             // deepbook::pool::Pool — exact type per DeepBook v3
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

- Legal in `Settling` (and in `Active` for mid-round premium conversion when
  `hold_premium_in_settlement == false`).
- Swaps `proceeds_settlement` → `U` with
  `min_out = pyth_value × (10⁴ − max_swap_slippage_bps) / 10⁴` — the crank chooses nothing.
- If `hold_premium_in_settlement == true`, finalize values residual `S` via Pyth instead of
  requiring a swap (see §7.4) and only exercise proceeds get swapped.

### 7.4 `finalize_round` — the accounting heart

```move
public fun finalize_round<U, S, V>(vault, treasury: &mut Treasury,
    underlying_info, settlement_info, clock, ctx)
```

Preconditions: `phase == Settling`, `positions_head == positions_tail` (all redeemed),
`open_rfqs == 0`, `proceeds_settlement` empty (or valued via Pyth under the hold-premium
policy), and `now ≥ current_expiry_ms` (vacuous at genesis).

Let, in underlying smallest-units:

```
aum        = deployable.value()                  (post-redeem, post-swap; excludes pending_deposits,
                                                  withdrawal_pool, and queued shares' backing — they're
                                                  all in deployable, see below)
shares     = total share supply + queued_withdraw_shares.value()   (escrowed shares still own P&L)
pps_prev   = pps[round − 1]                      (PPS_SCALE at genesis)
pps_gross  = aum × PPS_SCALE / shares            (u128, floor)         [shares == 0 ⇒ pps_gross = pps_prev]
```

**Fees** (only if `pps_gross > pps_prev`, i.e. the round was profitable in underlying terms):

```
mgmt_fee = aum × mgmt_fee_bps_annual × round_ms / (10⁴ × YEAR_MS)
perf_fee = premium_in_underlying × perf_fee_bps / 10⁴
profit   = aum − (shares × pps_prev / PPS_SCALE)     // round profit in underlying units
fees     = min(mgmt_fee + perf_fee, profit)          // cap: fees never push pps below pps_prev
```

where `premium_in_underlying` is `round_premium_collected` converted at the round's
swap execution (tracked when `swap_proceeds` runs) or at Pyth spot under hold-premium.
Fees transfer to the protocol `Treasury` (`treasury::deposit_balance` — same package).
Emits `VaultFeesCharged { mgmt_fee, perf_fee }` (zeroes on unprofitable rounds).

**Lock the round price**:

```
pps[round] = (aum − fees) × PPS_SCALE / shares          (floor; shares == 0 ⇒ pps_prev)
```

**Process queues, in this order** (all at `pps[round]`):

1. Withdrawals: `owed = queued_withdraw_shares.value() × pps[round] / PPS_SCALE`; move `owed`
   from `deployable` → `withdrawal_pool`; burn the escrowed shares via `share_treasury`.
2. Deposits: join `pending_deposits` into `deployable`. **Receipt round convention**: a
   `DepositReceipt` with `round = r` enters the vault at the start of round `r`, which is
   priced by `pps[r − 1]` — the value just locked by this finalize. So `claim_shares`
   converts at `pps[receipt.round − 1]`, and the receipt is claimable as soon as that
   entry exists. Keep this rule in one helper + one dedicated test so the off-by-one
   can't creep in (and mirror the same helper in `vault-sim::ledger`).
3. `round += 1`; `phase = Active`; reset per-round counters; `current_bucket = None`.

Emits `VaultRoundFinalized { round, pps, aum, shares, fees… }`.

### 7.5 `select_bucket`

```move
public fun select_bucket<U, S, V, C>(vault, bucket: &Bucket<U,S,C>,
    underlying_info, settlement_info, clock, ctx)
```

- Legal when `phase == Active` and `current_bucket.is_none()`.
- Validates: bucket types match the vault's `U`/`S` (type system), not invalidated,
  `now + min_expiry_lead ≤ bucket.expiry_ms ≤ now + max_expiry_lead`, strike within the
  §6 band.
- Sets `current_bucket`, `current_expiry_ms`, `selling_ends_ms = now + selling_window_ms`
  (capped at `expiry − rfq_max_extension − SETTLE_BUFFER`).
- Emits `VaultBucketSelected { round, bucket_id, strike, strike_scale, expiry_ms, spot }`.

### 7.6 `open_rfq`

```move
public fun open_rfq<U, S, V, C>(vault, bucket: &Bucket<U,S,C>, slice_amount: u64,
    underlying_info, settlement_info, clock, ctx)
```

- Legal when `phase == Active`, `current_bucket == Some(bucket)`, `now < selling_ends_ms`,
  `open_rfqs < max_open_rfqs`, `0 < slice_amount ≤ min(max_slice_amount, deployable.value())`.
- Computes the §6 reserve from Pyth; splits `slice_amount` out of `deployable`; calls
  `rfq::create` with the vault's RFQ params, `position_recipient = proceeds_recipient =`
  vault-ID-address, `origin =` vault ID; `open_rfqs += 1`.
- Slicing strategy (how many slices, when) is keeper-side policy (doc 04 §4) — the contract
  only enforces the caps.

### 7.7 `settle_rfq`

```move
public fun settle_rfq<U, S, V, C>(vault, rfq: RfqAuction<U,S,C>, bucket: &mut Bucket<U,S,C>,
    config: &ProtocolConfig, treasury: &mut Treasury, clock, ctx)
```

- Requires `rfq.origin == vault id` (only vault-originated RFQs flow back in here).
- Calls `rfq::finalize`:
  - **Winner**: `skim_fee` (protocol fee) → `bucket::write_collateralized` with the escrowed
    collateral → `Coin<Call>` to the winner's `call_recipient`; `Position` pushed into the
    vault's FIFO; net premium into `proceeds_settlement`;
    `round_premium_collected += net`. Emits `RfqSettled` (from rfq module).
  - **No winner**: collateral rejoins `deployable`; emits `RfqExpiredUnsold`.
- `open_rfqs −= 1`.
- If `hold_premium_in_settlement == false`, the premium waits in `proceeds_settlement` for a
  `swap_proceeds` crank (allowed mid-round).

## 8. Worked round math (normative example)

Underlying decimals 9 (SUI). `PPS_SCALE = 1e12`. Round 5, `pps[4] = 1.05e12`.

- Start of round 5: `deployable = 1_000e9`, supply+queued = `952.38e9` shares.
- Vault sells 1 000 SUI of calls, K = 1.30 × spot, collects net premium 4 200 USDC;
  swap executes at 3.5 USDC/SUI → `premium_in_underlying = 1 200e8...` *(illustrative)*.
- Expiry: cursor crosses 40% of the vault's range ⇒ redeem returns 600 SUI +
  400 × K USDC; swap at expiry spot → total `aum = 1_007.4e9`.
- `pps_gross = 1_007.4e9 × 1e12 / 952.38e9 = 1.05778e12 > pps[4]` ⇒ profitable ⇒
  `mgmt = 1_007.4e9 × 200 × 604_800_000 / (10⁴ × 31_536_000_000) ≈ 0.387e9`,
  `perf = premium_u × 1000 / 10⁴`.
- `pps[5] = (aum − fees) × 1e12 / shares`; withdrawals and the next round's deposits process
  at `pps[5]`.

The backtester (doc 06) reimplements exactly these integer formulas in its `ledger` module —
**same rounding, same order of operations** — so simulated and on-chain accounting can be
diffed unit-for-unit.

## 9. Admin surface (`AdminCap`-gated, funds-untouchable)

```move
public fun create_vault<U, S, V>(_: &AdminCap, share_treasury: TreasuryCap<V>,
    config: VaultConfig, ctx)                          // requires zero supply, like create_bucket
public fun update_config<U, S, V>(_: &AdminCap, vault, new: VaultConfig)   // bounds-checked; emits event
public fun pause_deposits / unpause_deposits
```

`update_config` must clamp: fees ≤ hard caps (`mgmt ≤ 500`, `perf ≤ 3000`), strike band sane
(`min < max`), slippage ≤ 500 bps. Config changes take effect next round (stash as
`pending_config`, applied in `finalize_round`) so an admin cannot change rules mid-flight.

## 10. Events & errors

Events: `VaultCreated`, `VaultDeposit`, `SharesClaimed`, `WithdrawInitiated`,
`WithdrawCompleted`, `InstantWithdraw`, `VaultBucketSelected`, `VaultPositionRedeemed`,
`VaultProceedsSwapped`, `VaultFeesCharged`, `VaultRoundFinalized`, `VaultConfigUpdated`,
`VaultDepositsPaused/Unpaused`. (Field lists follow the patterns above; indexer mapping in
doc 05 §1.)

Errors (continue the sequence from doc 02): `vault_wrong_phase(35)`,
`vault_bucket_not_selected(36)`, `vault_bucket_already_selected(37)`,
`vault_selling_closed(38)`, `vault_positions_pending(39)`, `vault_rfqs_open(40)`,
`vault_round_not_finalized(41)`, `vault_receipt_round_mismatch(42)`,
`vault_strike_out_of_band(43)`, `vault_expiry_out_of_band(44)`, `vault_slice_too_large(45)`,
`vault_too_many_rfqs(46)`, `vault_deposits_paused(47)`, `vault_wrong_origin(48)`,
`oracle_feed_mismatch(49)`, `oracle_price_stale(50)`, `oracle_confidence(51)`,
`vault_proceeds_unswapped(52)`.

## 11. Invariants (each becomes a Move test or property test)

1. `deployable + Σ open-RFQ escrows + Σ unredeemed position collateral` accounts for every
   underlying unit not in `withdrawal_pool`/`pending_deposits`.
2. `withdrawal_pool` ≥ Σ outstanding `WithdrawReceipt` obligations at their locked PPS
   (exact equality up to per-receipt floor dust).
3. Share supply changes only via `claim_shares` (mint) and finalize's queued-withdraw burn.
4. `pps[r]` is set exactly once, at finalize, and never mutated.
5. A depositor who deposits and immediately initiates withdrawal after one round receives
   `amount × pps[r]/pps[r−1]` — no path gains or loses value through the queues themselves.
6. Round P&L accrues only to shares live during the round (receipts queued for round r+1
   are inert for round r).
7. No function other than `complete_withdraw`/`instant_withdraw_pending` ever transfers
   underlying out of the vault; nothing transfers to `ctx.sender()` except explicit returns
   to the calling user.
8. Every phase is exit-able permissionlessly: for any reachable state there exists a public
   crank sequence to the next round (no admin in any liveness path). Test the stuck-cases:
   zero bids all round, zero deposits, all-shares-withdrawn, bucket invalidated mid-round.

## 12. Step-by-step

1. `oracle.move` + Move.toml Pyth dep; vectors shared with `mm-bot` spot math.
2. `vault.move` types + user functions (deposit/claim/withdraw trio) with `pps` table stubbed
   (manual setter under `#[test_only]`) — test the share math in isolation first.
3. Lifecycle cranks in order: `finalize_round` (genesis path) → `select_bucket` → `open_rfq`
   → `settle_rfq` → `crank_redeem` → `swap_proceeds` stub → full-loop test with the
   test-token pair and a scripted MM bidder.
4. Fees + config update + pause.
5. Invariant/property tests (§11), including multi-round randomized sequences.
6. `sui-tx` builders for every crank (doc 05 §5) — keeper and tests share them.
