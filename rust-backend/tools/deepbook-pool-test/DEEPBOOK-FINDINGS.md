# DeepBook v3 Testnet Findings (SO-155)

Verified 2026-06-09 against the **deployed** Sui-testnet DeepBook package via
read-only RPC only (`sui_getNormalizedMoveModule`, `suix_queryEvents`,
`sui_devInspectTransactionBlock`, `sui_getObject`). No docs were trusted where
deployed code could answer. Committed fixtures: real captured events under
`fixtures/`.

- Upgraded package (Move calls target this): `0x22be4cade64bf2d02412c7e8d0e8beea2f78828b948118d46735315409371a3c`
- Original package (ALL event/struct type strings resolve here): `0xfb28c4cbc6865bd1c897d26aecbe1f8792d1509a20ffec692c800660cbec6982`
- Package `constants::current_version()` = 5.

## A. Taker fees on permissionless pools — GO

**Takers do NOT need DEEP.** `pay_with_deep: bool` is an explicit caller
choice and input-token fees are live on testnet:

- Live fill on the permissionless SUI/DBUSDC pool (`fixtures/order_filled.testnet.json`,
  tx `9BCzvK5qXZDgCyGRQdVsFJKRpVfmSjWsS6fZM33XdHWZ`): `taker_fee_is_deep: false`
  while the maker side of the same fill has `maker_fee_is_deep: true` — the
  flag is per-participant, per-fill.
- Penalty: `constants::fee_penalty_multiplier()` dev-inspects to
  `1_250_000_000` = **1.25×** (float scaling 1e9 = 1.0). The fixture confirms
  the math: taker bid, `quote_quantity = 755000`, pool taker rate 0.1% →
  `755000 × 0.001 × 1.25 = 943.75` and the event's charged `taker_fee = 943`
  (in the input token, quote units).
- Per-pool default fees (read from live permissionless pool state and from
  every recent `PoolCreated`): `taker_fee = 1_000_000` (0.1%),
  `maker_fee = 500_000` (0.05%). There are no global taker/maker defaults in
  `constants` — fees live in pool state. EWMA add-on bounds:
  `default_additional_taker_fee` 0.1%, max 0.2%.
- Dedicated input-fee quote helpers exist: `get_quantity_out_input_fee`,
  `get_base_quantity_out_input_fee`, `get_quote_quantity_out_input_fee`.

So: UI/bot traders pay fees in the token they're already spending, at 1.25×
the DEEP-denominated rate (effective taker ~0.125%). Acceptable; no DEEP
distribution problem.

Residual check (cheap, during SO-157): one dev-inspect of
`swap_exact_quote_for_base` with `coin::zero<DEEP>` against a permissionless
pool, to confirm the zero-DEEP path of the *coin-flavored* swaps specifically
(the BalanceManager path is fully confirmed by the live fill above).

## B. Events

### OrderFilled — NOT generic (dispatcher can exact-match it)

Type string: `0xfb28c4…6982::order_info::OrderFilled` (module `order_info`,
zero type params — `suix_queryEvents` with an exact `MoveEventType` filter
works and is how the fixture was captured). Fields:

```
pool_id: ID, maker_order_id: u128, taker_order_id: u128,
maker_client_order_id: u64, taker_client_order_id: u64, price: u64,
taker_is_bid: bool, taker_fee: u64, taker_fee_is_deep: bool,
maker_fee: u64, maker_fee_is_deep: bool, base_quantity: u64,
quote_quantity: u64, maker_balance_manager_id: ID,
taker_balance_manager_id: ID, timestamp: u64
```

`taker_fee`/`maker_fee` are charged **amounts** (not rates), denominated in
DEEP when the matching `*_fee_is_deep` is true, else in that side's input
token. `timestamp` is unix-ms. One event per maker order crossed. Siblings in
the same module: `OrderPlaced`, `OrderCanceled`, `OrderExpired`,
`OrderFullyFilled` (and the full `OrderInfo` struct is itself emitted).

**SO-156 watcher**: filter `MoveEventType = {original_pkg}::order_info::OrderFilled`,
then client-side filter `parsedJson.pool_id` against our watched pools.

### PoolCreated — generic (dispatcher needs prefix match)

Type string: `0xfb28c4…6982::pool::PoolCreated<Base, Quote>` (2 type params,
concrete coin types in the string — see `fixtures/pool_created.testnet.json`).
BCS/JSON fields (base/quote types are ONLY in the type string, not the payload):

```
pool_id: ID, taker_fee: u64, maker_fee: u64, tick_size: u64, lot_size: u64,
min_size: u64, whitelisted_pool: bool, treasury_address: address
```

Confirms the SO-152 design: prefix-match on
`{original_pkg}::pool::PoolCreated<`, parse the two type params from the
string.

## C. Price scaling — CONFIRMED

`price_real = price_raw / 10^(9 - base_decimals + quote_decimals)`.
Fixture check: SUI(9 dec)/DBUSDC(6 dec) fill, `price = 755000`, scaling
`10^(9-9+6)=10^6` → $0.755/SUI. ✓

## D. BalanceManager

Module `balance_manager` on the original package id.

- **Create**: `new(ctx): BalanceManager` returns a value; the module has NO
  `share` function — the PTB must call `0x2::transfer::public_share_object`.
  Variants: `new_with_owner`, `new_with_custom_owner`,
  `new_with_custom_owner_and_caps(addr, ctx): (BM, DepositCap, WithdrawCap, TradeCap)`.
- **Discovery**: `BalanceManagerEvent { balance_manager_id, owner }` exists
  but `new` does NOT emit it (zero instances on all of testnet). It is tied to
  the optional `register_balance_manager(&BalanceManager, &mut Registry, ctx)`.
  → **SO-157 design**: the creation PTB should be
  `new` → `register_balance_manager` → `public_share_object`, giving durable
  on-chain discovery (queryEvents by `MoveEventType = …::balance_manager::BalanceManagerEvent`,
  filter `owner` client-side), with localStorage as a cache, not the source of
  truth. `BalanceEvent { balance_manager_id, asset, amount, deposit }` fires on
  every deposit/withdraw (secondary recovery signal).
- **Proofs/funds**: `generate_proof_as_owner(&mut BM, &ctx): TradeProof`;
  `deposit<T>(&mut BM, Coin<T>, ctx)`; `withdraw<T>(&mut BM, u64, ctx): Coin<T>`;
  `withdraw_all<T>`; views `balance<T>(&BM): u64`, `owner(&BM): address`, `id(&BM): ID`.

## E. Exact order/swap signatures (deployed v5)

```
pool::place_limit_order<B,Q>(
  &mut Pool, &mut BalanceManager, &TradeProof,
  client_order_id: u64, order_type: u8, self_matching_option: u8,
  price: u64, quantity: u64, is_bid: bool,
  pay_with_deep: bool,            // param 10
  expire_timestamp: u64, &Clock, &TxContext
): OrderInfo                      // public (not entry), droppable

pool::place_market_order<B,Q>(
  &mut Pool, &mut BalanceManager, &TradeProof,
  client_order_id: u64, self_matching_option: u8,
  quantity: u64, is_bid: bool,
  pay_with_deep: bool,            // param 8
  &Clock, &TxContext
): OrderInfo

pool::cancel_order(&mut Pool, &mut BM, &TradeProof, order_id: u128, &Clock, &ctx)
pool::cancel_orders(…, vector<u128>, …)
pool::cancel_all_orders(&mut Pool, &mut BM, &TradeProof, &Clock, &ctx)
pool::withdraw_settled_amounts(&mut Pool, &mut BM, &TradeProof)
pool::withdraw_settled_amounts_permissionless(&mut Pool, &mut BM)   // no proof

// reads (devInspect)
pool::account_open_orders(&Pool, &BM): VecSet<u128>
pool::get_level2_range(&Pool, lo: u64, hi: u64, is_bid: bool, &Clock): (vector<u64>, vector<u64>)
pool::get_level2_ticks_from_mid(&Pool, ticks: u64, &Clock): (vec<u64> ×4)
pool::mid_price(&Pool, &Clock): u64

// coin-flavored swaps (no BM) — DEEP coin param may be zero (input-token fee)
pool::swap_exact_quote_for_base<B,Q>(&mut Pool, Coin<Q>, Coin<DEEP>, min_base_out: u64, &Clock, ctx)
  : (Coin<B>, Coin<Q>, Coin<DEEP>)
pool::swap_exact_base_for_quote<B,Q>(&mut Pool, Coin<B>, Coin<DEEP>, min_quote_out: u64, &Clock, ctx)
  : (Coin<B>, Coin<Q>, Coin<DEEP>)
pool::swap_exact_quantity<B,Q>(&mut Pool, Coin<B>, Coin<Q>, Coin<DEEP>, min_out, &Clock, ctx)
// *_with_manager variants exist but need TradeCap+DepositCap+WithdrawCap — not for the UI path.

pool::create_permissionless_pool<B,Q>(&mut Registry, tick: u64, lot: u64, min: u64,
  creation_fee: Coin<DEEP> /* exactly 500 DEEP */, ctx): ID
```

## F. Decided PTB shapes for SO-157 / SO-158

- **Enable trading (one-time)**: `balance_manager::new` →
  `balance_manager::register_balance_manager(bm, Registry)` →
  `0x2::transfer::public_share_object(bm)`.
- **Limit order**: `coinWithBalance(input)` → `balance_manager::deposit<T>` →
  `generate_proof_as_owner` → `place_limit_order(…, pay_with_deep=false, …)`.
- **Market order**: same BM path with `place_market_order(…, pay_with_deep=false)`
  (keeps one infra for both order types; slippage guarded in UI from
  `get_level2_range` depth). The coin-flavored `swap_exact_*` with
  `coin::zero<DEEP>` stays the fallback shape if BM-less market orders are
  ever wanted.
- **Cancel / settle**: `generate_proof_as_owner` → `cancel_order` /
  `cancel_all_orders`; settled funds via `withdraw_settled_amounts` then
  `balance_manager::withdraw<T>` to the wallet.
- **mm-bot**: same shapes in Rust; cancel-all + re-place composes in one PTB
  (all functions are `public`, non-entry, on shared objects).

## G. Fee/constants reference (dev-inspect, decoded)

| constant | value | meaning |
|---|---|---|
| `pool_creation_fee()` | 500_000_000 | 500 DEEP (deep_unit 1e6) |
| `fee_is_deep()` | true | default flag only |
| `fee_penalty_multiplier()` | 1_250_000_000 | 1.25× input-token fee penalty |
| `default_additional_taker_fee()` | 1_000_000 | 0.1% EWMA add-on default |
| `max_additional_taker_fee()` | 2_000_000 | 0.2% cap |
| `default_stake_required()` | 100_000_000 | 100 DEEP (governance) |
| `float_scaling()` | 1_000_000_000 | 1.0 |
| `current_version()` | 5 | deployed package version |

Whitelisted DEEP/SUI pool (`0x48c959…`): taker/maker 0 — why the zero-DEEP
SUI→DEEP swap worked. Permissionless pools observed: taker 0.1% / maker 0.05%.
