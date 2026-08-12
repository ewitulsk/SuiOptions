# 01 — P0: The `bounded-curator` guard contracts

Two new Move packages, zero changes to existing packages (see 00 §delta-1):

```
contracts/bounded-curator/            package name: bounded_curator
  Move.toml                           deps: trading_vault (../trading-vault), options_core (../core)
  sources/guard.move                  GuardedCuratorCap, OwnerCap, wrap/unwrap, mirrored cap functions
  sources/limiter.move                VaultLimiter (shared), TradePolicy, band + notional checks
  sources/errors.move
  sources/events.move
  tests/…

contracts/guarded-exchange-adapter/   package name: guarded_exchange_adapter
  Move.toml                           deps: exchange (../exchange), trading_vault, options_core, bounded_curator
  sources/guarded_exchange_adapter.move   fork of exchange_adapter with limiter enforcement at fill time
```

Why two packages: the guard must depend on `trading_vault` only, so it can be frozen at P5 without freezing exchange coupling; the guarded adapter depends on `exchange` and will iterate with it. They version independently.

## 1. Objects

```move
// sources/guard.move
public struct GuardedCuratorCap has key, store {
    id: UID,
    vault_id: ID,
    inner: CuratorCap,            // the real cap — vault.curator_cap_id points at *inner's* id
    limiter_id: ID,               // the shared VaultLimiter for this vault
}

public struct OwnerCap has key, store {
    id: UID,
    vault_id: ID,
    guard_id: ID,
}

// sources/limiter.move — SHARED, because fills are permissionless txs that
// don't carry the guard object (00 §delta-4).
public struct VaultLimiter has key {
    id: UID,
    vault_id: ID,
    policy: TradePolicy,
    spent_this_epoch: u64,        // notional in accounting-asset raw units
    last_epoch: u64,              // ctx.epoch()
}

public struct TradePolicy has store, copy, drop {
    price_band_bps: u64,          // deviation allowed vs PriceAttestation mark
    max_notional_per_epoch: u64,  // accounting-asset raw units; the primary limiter
    allowed_markets: VecSet<ID>,  // SettlementRegistry ids + DeepBook pool ids
    mm_release_allowed: bool,     // default false — see §4
}
```

Key subtlety: `assert_current_cap` (`trading-vault/sources/vault.move:1546`) checks `object::id(cap) == vault.curator_cap_id`. Wrapping does not change the inner cap's object id, so **a wrapped cap keeps working with no vault-side changes** — the guard just borrows `&guard.inner` and forwards.

## 2. Wrap / unwrap / creation flow

```move
/// Create a studio vault in one PTB. Caller (the provisioner-funded curator
/// wallet) ends up holding the GuardedCuratorCap; `owner` (the user's wallet)
/// gets the OwnerCap; the limiter is shared.
public fun create_guarded_vault<T>(
    cfg: &VaultProtocolConfig,
    lockup_ms: u64, curator_fee_bps: u64, unwind_grace_ms: u64,
    policy: TradePolicy,
    owner: address,
    ctx: &mut TxContext,
): ID
```

Implementation note: `vault::create_vault<T>` transfers the fresh `CuratorCap` to `ctx.sender()` (`vault.move:311-325`) — it does not return it. So `create_guarded_vault` cannot be a single Move call; it is a **two-transaction ceremony** executed by the provisioner with the curator key:

1. Tx 1: `vault::create_vault<T>(…)` → curator wallet owns the raw cap.
2. Tx 2: `bounded_curator::wrap(cap, policy, owner, ctx)` — consumes the owned cap by value, mints `GuardedCuratorCap` (kept by sender) + `OwnerCap` (transferred to `owner`), shares the `VaultLimiter`, emits `GuardCreated`.

(If you want single-PTB creation later, add a `create_vault_returning_cap` to `trading_vault` — a core change, deliberately out of P0 scope.)

`wrap` must also perform setup that must never be re-doable by the bot:

```move
public fun wrap(cap: CuratorCap, vault: &mut TradingVault, reg: &IntegrationRegistry,
                policy: TradePolicy, owner: address, ctx: &mut TxContext)
```
- `vault::add_quote_adapter<GuardedExchangeAdapter>(vault, &cap)` — opt in ONLY the guarded adapter.
- assert the stock `ExchangeAdapter` is **not** in the vault's `quote_adapters` (abort if a pre-opted vault is being wrapped).
- `vault::set_mm_release_enabled(vault, &cap, false)`.

**OwnerCap functions** (the user-wallet surface — each needs a gas-station template, see 09 §6):

```move
public fun unwrap(owner: OwnerCap, guard: GuardedCuratorCap, recipient: address)
    // destroys both, transfers the raw CuratorCap to recipient. Full exit from the guard.
public fun rotate_curator(owner: &OwnerCap, guard: &GuardedCuratorCap,
                          vault: &mut TradingVault, recipient: address, ctx: &mut TxContext)
    // calls vault::rotate_curator_by_curator via the inner cap → mints a fresh RAW cap
    // to `recipient`, disowning the guard's inner cap. This is REVOKE: the guard object
    // becomes inert (assert_current_cap now fails for inner). Emits GuardRevoked.
public fun set_policy(owner: &OwnerCap, limiter: &mut VaultLimiter, policy: TradePolicy)
```

Note `rotate_curator` deliberately leaves a dead guard object behind rather than trying to reclaim it — the guard is owned by the bot wallet, which the owner cannot touch. Inertness is the revocation; the dashboard detects it by comparing `vault.curator_cap_id` to the guard's inner id.

## 3. The mirrored surface — decision table for all 41 cap-gated functions

Deny-by-default: anything unmirrored is unreachable by the bot. Decisions per the inventory in `vault.move` / `vault_mm.move` / `deepbook_adapter.move` / `exchange_adapter.move`:

| Function (module:line) | Mirror? | Guard treatment |
|---|---|---|
| `vault::deposit_as_curator` (:350) | ✅ | pass-through (deposits into the vault are always safe) |
| `vault::add_deposit_asset` (:459) / `remove_deposit_asset` (:479) | ✅ | pass-through |
| `vault::set_haircuts` (:489) | ✅ | clamp: entry/exit ≤ 500 bps |
| `vault::request_withdraw_as_curator` (:532) | ✅ | pass-through — it already enforces the curator share floor, and the stake is cap-keyed so the exit path must stay reachable |
| `vault::begin_session` (:876) | ❌ | **never** — raw take-capable sessions are the drain primitive. Bots trade only through the guarded adapter + guarded DeepBook mirrors |
| `vault::add_quote_adapter` (:951) / `remove_quote_adapter` (:959) | ❌ | set once at `wrap`; owner-gated variant only |
| `vault::set_external_account_attested` (:1312) / `release_external` (:1378) | ❌ v1 | external accounts out of studio scope; revisit at P4 |
| `vault::initiate_close` (:1476) | ✅ | pass-through (winding down is depositor-favorable) |
| `vault::rotate_curator_by_curator` (:1509) | ❌ | **the escape hatch** — owner-gated only (`rotate_curator`, §2) |
| `vault::set_deposits_paused` (:1531) | ✅ | pass-through |
| `vault::set_mm_release_enabled` (:1538) | ❌ | owner-gated, and only if `policy.mm_release_allowed` |
| `vault_mm::exercise_call_coin` / `exercise_put_coin` / `close_offset_position` / `close_offset_put_position` / `release_coin_to_balances` (:181-:363) | ✅ | pass-through — position maintenance, no pricing freedom |
| `deepbook_adapter::init_custody` (:115), `deposit` (:151), `withdraw` (:170) | ✅ | pass-through; `deposit` counts toward epoch notional |
| `deepbook_adapter::place_limit_order` (:192) | ✅ | **banded**: require a fresh `PriceAttestation` for the pool's base/quote; assert `price` within `price_band_bps` of the attested mark; assert pool id ∈ `allowed_markets`; add `price × quantity` to the limiter |
| `deepbook_adapter::place_market_order` (:235) | ❌ v1 | no price bound expressible; use banded taker swaps instead |
| `deepbook_adapter::modify_order` (:272) / `cancel_order` (:291) / `cancel_all_orders` (:309) / `withdraw_settled` (:327) / `retire_pool` (:345) / `eject_empty_custody` (:366) | ✅ | pass-through |
| `deepbook_adapter::taker_swap_base_for_quote` / `taker_swap_quote_for_base` (:407/:447) | ✅ | **banded via min_out**: guard computes the minimum acceptable `min_out` from the attestation ± band and asserts the caller's `min_out` meets it; notional counted |
| `exchange_adapter::*` (all 8, :111-:259) | ❌ | replaced wholesale by `guarded_exchange_adapter` (§5); the stock adapter is never opted in |
| `options_adapter::custody_position_for_testing` (:58) | ❌ | test helper |

Every mirror takes `(guard: &GuardedCuratorCap, limiter: &mut VaultLimiter, …original args…)`, asserts `limiter.id == guard.limiter_id`, rolls the epoch (`if ctx.epoch() > last_epoch { spent = 0 }`), enforces, then forwards with `&guard.inner`.

## 4. Pricing marks for the band

Band checks consume `trading_vault::price::PriceAttestation` (`price.move:24`, 1e12 scale) — the same normalized struct the whole protocol uses, produced by `oracle_pyth::attest` / `oracle_switchboard::attest` against the `OracleRegistry` allowlist+pin. **The guard therefore inherits the Switchboard↔Pyth flip with zero code** (spec D9): the gateway assembles whichever adapter's attest call is active into the PTB, and the guard only ever sees the attestation.

- Freshness: reuse `vault::check_attestation(vault, cfg, &att, clock)` (`vault.move:1245`) or assert `clock.timestamp_ms() - att.timestamp_ms() <= max_price_age_ms` directly. Keep the guard's own bound ≤ the protocol's 60s default.
- Spot pairs (DeepBook, spot legs): attestation of base priced in quote — direct comparison.
- Option marks (exchange markets): v1 uses the **spot-anchored coarse band** — attest the underlying, derive an intrinsic-value floor plus a generous premium ceiling (`price_band_bps` wide, default 1500–3000 bps). This blocks the sell-at-1%-of-value drain without an on-chain vol model; the notional cap is the primary limiter (spec §6.3). `options_adapter::options_oracle::attest_call/attest_put` (intrinsic + `vol_book`) is available if a tighter mark is wanted later.

## 5. `guarded_exchange_adapter`

A fork of `contracts/exchange-adapter/sources/exchange_adapter.move` (~700 lines) with its own witness type. Forking, not wrapping, is forced by the witness model: `vault::begin_quote_session<W>` bakes the adapter's type into the session, so a wrapper package can't reuse the stock adapter's sessions.

Changes from stock:

1. Witness/struct rename: `GuardedExchangeAdapter`, `GuardedExchangeCustody` (same shape: `{id, vault_id, bm_id, owner_cap: OwnerCap, direct, assets}` where `OwnerCap` here is `exchange::balance_manager::OwnerCap`).
2. Cap-gated setup functions (`init_direct_custody`, `fund`, `defund`, `track_asset`, `add_signer`, `remove_signer`, `eject_empty_custody`) take `guard: &GuardedCuratorCap` instead of `cap: &CuratorCap` and forward `&guard.inner`. `fund` counts toward epoch notional (`limiter: &mut VaultLimiter` param). `add_signer` emits an event the dashboard surfaces (a new signer = a new key that can quote the vault).
3. Fill functions (`fill_vault_order`, `fill_vault_order_reverse`, `match_vault_vs_bm`, `match_bm_vs_vault`, `match_vault_vs_vault`) gain two parameters: `limiter: &mut VaultLimiter` and `att: PriceAttestation` (plus `cfg: &VaultProtocolConfig`, `clock: &Clock` for freshness). Before settling:
   - assert `limiter.vault_id == vault_id_of_this_custody`;
   - assert the market (`object::id(settlement_registry)`) ∈ `policy.allowed_markets`;
   - assert order price within band of the attested mark (§4);
   - roll epoch, assert `spent_this_epoch + fill_notional ≤ max_notional_per_epoch`, accumulate;
   - then run the stock escrow flow (`settlement::begin_fill…finish`).
   `match_vault_vs_vault` checks **both** vaults' limiters when both are studio vaults.
4. Registration: the activation PTB must `registry::allow_adapter` the new witness (see §6). The orderbook's `Submitter` must learn to target `guarded_exchange_adapter::match_*` for studio custodies — see 02 §6.

**Who supplies the attestation on a permissionless fill?** The orderbook relayer (matched mode) or the taker (open-orderbook mode) builds the PTB; both already assemble oracle calls elsewhere in the stack. The `/v1/routes` PTB skeleton and the Submitter grow an oracle-attest prefix for studio-vault legs. A fill with a stale/absent attestation aborts — mapped in the Submitter's `decode_abort` to a `Stale`-class outcome (restore + re-match), not a prune.

## 6. Publish-pipeline wiring

`rust-backend/tools/deployment-manager`:

1. `src/main.rs` — add `publish_dep_package(...contracts_root.join("bounded-curator"), "bounded_curator", …)` **after** trading-vault (step 2) and `guarded-exchange-adapter` **after** exchange-adapter (step 11). Follow the `Published.toml` stash/finish discipline already handled by `crates/move-publish`.
2. `src/json_store.rs` — two new `Option<PackageRecord>` fields on `PackageInfo`: `boundedCurator`, `guardedExchangeAdapter` (camelCase serde).
3. `src/trading_vault_init.rs::activate` — add `registry::allow_adapter` for `guarded_exchange_adapter::GuardedExchangeAdapter`.
4. `crates/deployments` + `crates/token-info-client` — mirror the new fields; accessors `bounded_curator()`, `guarded_exchange_adapter()`.
5. Remember (memory: contracts publish pipeline): dep-replacement address overrides are ignored — the publish reads each dep's own `Published.toml`; publish order above guarantees they exist.

## 7. P0 validation drills (staging)

Script these as an integration test or `tools/` binary; they are the phase gate.

| Drill | Expect |
|---|---|
| Wrap ceremony: create vault → wrap → owner receives `OwnerCap`, limiter shared, stock adapter not opted in | pass |
| Bot places banded DeepBook limit order inside band | success |
| Bot places order 50% off attested mark | abort `E_PRICE_OUT_OF_BAND` |
| Self-trade drain attempt through guarded exchange fill at garbage price (attacker-signed taker) | abort at fill |
| Notional cap: orders summing past `max_notional_per_epoch` in one epoch | (n+1)th aborts; next epoch resets |
| `begin_session` directly with the wrapped cap from the bot wallet | impossible — cap not owned; guard exposes no session |
| `rotate_curator_by_curator` from bot | not mirrored — unreachable |
| Owner `rotate_curator` from user wallet | fresh raw cap delivered; guard inert; bot's next guarded call aborts `not_curator` |
| Owner `unwrap` | raw cap delivered; guard + owner cap destroyed |
| Depositor `request_withdraw` + permissionless fulfillment with the bot **stopped** | redemption completes |
| Oracle flip rehearsal: run drills with `provider = "pyth"` attestations | identical behavior |
