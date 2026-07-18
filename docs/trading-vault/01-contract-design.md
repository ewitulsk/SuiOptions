# Curated Trading Vaults — Contract Design

**Status**: Draft v1 (2026-07-17)
**Package**: `contracts/trading-vault` (`trading_vault`) — fifth protocol package
**Depends on**: `options_core` (Treasury, AdminCap), `auction`/`options_rfq` (options adapter), DeepBook v3, Pyth (NAV pricing only)

---

## 1. Overview

A permissionless curated-vault product in the style of Hyperliquid vaults and
Morpho: anyone creates a vault, the creator names a curator, users deposit a
single per-vault asset, and the curator deploys funds into
protocol-allowlisted **integration adapters** (DeepBook spot first, our
options protocol second) through a hot-potato session interface that makes
"trade" possible and "withdraw to self" impossible.

This is a new product, distinct from the existing Ribbon-style covered-call
`options_vault`. The covered-call vault will **eventually merge into this
structure** (as a keeper-cranked strategy over the RFQ-writer adapter) but no
migration work is in scope now. The design must simply not preclude it — and
it doesn't: a curator is just a cap holder, which can be a keeper wallet.

### 1.1 Decisions log

| # | Decision | Choice |
|---|----------|--------|
| 1 | Deposit denomination | Set per vault at creation; exactly one asset per vault. All accounting (cost basis, NAV, fees) in that asset's units. |
| 2 | Protocol fee | Morpho-style: a configurable cut **of the curator's performance fee**, not an independent fee on the user. |
| 3 | Curator fee payout | Auto-compounded: minted as vault shares into the curator's stake. Protocol's cut paid in cash to the core `Treasury`. |
| 4 | Withdrawal liquidity | Full withdrawal-request queue in v1 (FIFO, permissionless fulfillment crank, force-unwind after grace period). |
| 5 | Curator identity | `CuratorCap` object, programmatically transferable; per-vault rotation authority: Creator / Curator / Either. |
| 6 | Oracle guardrails | **None on trading.** Curators trade with full freedom; no price bands, no oracle checks on orders. Pyth is used *only* as the NAV pricing source in appraisal. |
| 7 | Shares | Internal ledger with per-user cost basis. Non-transferable. No `Coin<VShare>` (permissionless creation can't mint OTW types; transferable shares would let profit escape the performance fee). |

---

## 2. Core objects

```move
/// Not generic — holds arbitrary asset types as dynamic fields (mm_collateral
/// pattern). The deposit asset is fixed per vault in config.
public struct TradingVault has key {
    id: UID,
    creator: address,
    curator_cap_id: ID,                  // the currently-valid CuratorCap
    state: VaultState,                   // Open | Closing | Closed
    config: VaultConfig,
    total_shares: u128,
    stakes: Table<StakeKey, Stake>,
    position_count: u64,                 // custody objects: dynamic object fields
    // free balances: df Balance<T> keyed by TypeName
    // withdrawal queue: Table<u64, WithdrawRequest> + head/tail (ring, like
    // options_vault positions table)
    queue_head: u64,
    queue_tail: u64,
}

public struct VaultConfig has store, copy, drop {
    deposit_asset: TypeName,             // decision 1: one asset, fixed at creation
    lockup_ms: u64,
    curator_fee_bps: u64,                // default 1000, capped by protocol config
    rotation_authority: u8,              // ROTATE_CREATOR | ROTATE_CURATOR | ROTATE_EITHER
    max_positions: u64,                  // bounds appraisal PTB size (~32–64)
    unwind_grace_ms: u64,                // queue age before force-unwind unlocks
    deposits_paused: bool,
}

/// Stakes are keyed by address for depositors, and by the CuratorCap id for
/// the curator role — the curator's skin-in-the-game travels with the cap.
public enum StakeKey has store, copy, drop { Addr(address), CuratorCap(ID) }

public struct Stake has store {
    shares: u128,
    cost_basis: u64,                     // deposit-asset units; reduced pro-rata on withdraw
    locked_until_ms: u64,
}

public struct CuratorCap has key, store { id: UID, vault_id: ID }

/// Shared, AdminCap-gated protocol knobs.
public struct VaultProtocolConfig has key {
    id: UID,
    min_curator_share_bps: u64,          // default 500
    enforce_curator_share: bool,         // protocol-level disablement
    max_curator_fee_bps: u64,            // e.g. 3000
    protocol_fee_bps: u64,               // decision 2: share OF the curator fee
    paused: bool,
}

/// Shared, AdminCap-gated adapter allowlist. Removal = instant kill switch
/// for new sessions on that adapter.
public struct IntegrationRegistry has key { id: UID, allowed: VecSet<TypeName> }
```

### 2.1 CuratorCap semantics

- The cap is freely transferable by its holder (`store`), so curators can
  hand the role to a bot wallet, a multisig, or a buyer. **The curator's
  stake is keyed by the cap's ID**, so skin-in-the-game moves with the role.
- `vault.curator_cap_id` pins which cap is live. The configured
  `rotation_authority` (creator, curator, or either) may call
  `rotate_curator(vault, auth, recipient)` which mints a fresh cap, updates
  `curator_cap_id` (dead caps become inert), and converts the old cap-keyed
  stake into an `Addr`-keyed stake for the old holder (normal lockup rules,
  crystallized on withdrawal like anyone else's).
- Every curator-gated function takes `&CuratorCap` and asserts
  `cap.vault_id == vault && object::id(cap) == vault.curator_cap_id`.

---

## 3. Custody invariant: sessions

Curator operations run inside a `Session` hot potato (no abilities — must
resolve in one transaction). This extends the codebase's established
`CollateralRequest`/`release` pattern.

```move
public fun begin_session<W: drop>(
    vault: &mut TradingVault, cap: &CuratorCap,
    reg: &IntegrationRegistry, _witness: W, ctx: &TxContext,
): Session   // asserts cap valid, vault Open (or Closing for unwind-only entry points),
             // TypeName<W> allowlisted

public fun take<T>(vault, &mut Session, amount: u64): Balance<T>
public fun put<T>(vault, &mut Session, b: Balance<T>)
public fun put_position<P: key + store>(vault, &mut Session, p: P)      // custody, tagged W
public fun take_position<P: key + store>(vault, &mut Session, id: ID): P // same-W tag only
public fun end_session(vault, s: Session)   // emits SessionSettled { net flows } for indexer
```

No vault function ever returns funds to the transaction sender. `take` hands
`Balance`s only to adapter code paths (witness-gated), and adapters by
construction deposit outputs back via `put`/`put_position`. Adapters **never
return `&mut` custody objects to the PTB** — all venue calls happen inside
adapter functions.

**Trust model (stated plainly):** depositor security reduces to (1) vault-core
invariants and (2) the audit of each allowlisted adapter — the Morpho model.
Move has no dynamic dispatch, so "100% generalizable" on Sui means: any team
ships an adapter package against this witness interface and the protocol
allowlists it. A perps venue or lending market later is a new adapter package
plus one registry entry; zero vault-core changes.

**Positions arriving by transfer** (e.g. RFQ settlement mints a `Position` to
a recipient address): recipients are set to the vault's own object address and
swept in permissionlessly via transfer-to-object receiving:
`receive_position(vault, Receiving<P>)`.

---

## 4. NAV and appraisal

Deposits and queue fulfillment need a fresh NAV in the same transaction.
Mechanism: an `Appraisal` hot potato, completeness-checked.

- `begin_appraisal(vault, clock)` → potato recording the set of held asset
  types and `position_count`.
- `appraise_balance<T>(vault, &mut Appraisal, &PriceInfoObject...)` — values
  each free `Balance<T>` in deposit-asset units. Pricing reuses the
  `spot_cross` two-feed cross (module copied from `options_vault::oracle`;
  extract a shared lib only when the covered-call merge happens). Staleness /
  confidence checks apply **to appraisal only** — they protect depositors'
  share price, they do not constrain the curator (decision 6).
- Each adapter exposes `appraise_*` for its position types (§6, §7).
- Consuming functions require a complete appraisal: all held types covered,
  positions appraised == `position_count`, same tx.

The frontend/backend builds the PTB from indexed vault state. `max_positions`
bounds PTB size. Deposit assets must have a Pyth feed (the `token_info`
catalog already maps symbol → `pythFeedId`).

**Share-inflation defenses**: virtual-share offset plus a minimum creator seed
deposit at `create_vault`.

`pps = NAV × PPS_SCALE / total_shares` (`PPS_SCALE = 1e12`, matching
`options_vault`).

---

## 5. Deposits, withdrawal queue, fees

### 5.1 Deposit

`deposit<T>(vault, appraisal, Coin<T>, clock)` — asserts `TypeName<T> ==
config.deposit_asset`, vault Open, not paused. `shares = amount ×
total_shares / NAV` (virtual offset at genesis); `stake.cost_basis += amount`;
`locked_until = now + lockup_ms`. Curator deposits (with cap in hand) credit
the cap-keyed stake.

### 5.2 Withdrawal queue (decision 4)

Two-step, FIFO, crystallizing at **fulfillment-time** pps:

1. `request_withdraw(vault, shares, clock)` — requires `now ≥ locked_until`
   (waived when Closed). Escrows the shares and a pro-rata slice of cost
   basis into `WithdrawRequest { key: StakeKey, shares, basis, requested_at_ms }`
   at `queue_tail`. **Curator floor check happens here**: for a cap-keyed
   request, assert post-request `curator_shares / total_shares ≥
   min_curator_share_bps`, skipped when `enforce_curator_share == false` or
   vault is Closing/Closed.
2. `fulfill_withdrawals(vault, appraisal, clock)` — permissionless crank;
   processes from `queue_head` while free deposit-asset balance covers each
   payout. Per request:

```
value        = shares × pps / PPS_SCALE
profit       = max(0, value − basis)
gross_fee    = profit × curator_fee_bps / 10_000
protocol_cut = gross_fee × protocol_fee_bps / 10_000     // decision 2
curator_net  = gross_fee − protocol_cut
payout       = value − gross_fee                          // cash to user
```

- `protocol_cut` → cash into `options_core::treasury::Treasury` (already
  keyed by `TypeName`, works for any deposit asset).
- `curator_net` → minted as shares at current pps into the cap-keyed stake
  (decision 3). This is pps-neutral for remaining depositors (the minted
  shares exactly offset the assets retained), auto-compounds, and organically
  supports the curator floor.
- Requests are all-or-nothing per request; the crank stops at the first
  request it cannot fund.

**Force-unwind**: if `now − head.requested_at_ms > unwind_grace_ms`,
permissionless per-adapter conservative unwind entry points unlock —
DeepBook: `cancel_all_orders` + withdraw all BM balances to the vault;
options: redeem expired positions. These are non-discretionary actions only
(no market-selling of inventory); they convert as much as possible to free
balance so the crank can progress. Curators are expected to service the queue
long before this.

### 5.3 Closure

`Closing` (set by curator via cap, or AdminCap): deposits blocked, sessions
admit only adapter unwind entry points. When `position_count == 0` and only
the deposit asset remains (curator swaps residual inventory via the DeepBook
adapter), `finalize_close` → `Closed`: lockups and curator floor waived,
every remaining stake is paid out through the same crystallization math,
permissionlessly per-stake so nobody is stranded.

---

## 6. DeepBook adapter

### 6.1 Custody — wrapped BalanceManager (spike result, 2026-07-17)

Findings from source review (`MystenLabs/deepbookv3`) and mainnet registry
inspection:

- `new_with_custom_owner_caps_v2<App: drop>` requires
  `registry.assert_app_is_authorized<App>()`; `authorize_app` is
  **`DeepbookAdminCap`-gated**. Mainnet registry
  (`0xaf16…549d`) has exactly one authorized app: DeepBook's own
  `margin_manager::MarginApp`. This path is closed without Mysten
  authorization (worth pursuing as BD later, not a dependency).
- **`BalanceManager has key, store`** — it can be wrapped. Chosen design:

1. `init_deepbook(vault, cap, session…)`: adapter calls
   `balance_manager::new(ctx)` (owner = tx sender for exactly this
   transaction), immediately mints `TradeCap`, `DepositCap`, `WithdrawCap`
   (sender == owner passes), then wraps the BM **and all three caps** into
   the vault as adapter-tagged dynamic object fields.
2. After wrapping, `&mut BalanceManager` is reachable only inside adapter
   functions. The owner-gated paths (`deposit`, `withdraw`, `mint_*_cap`,
   `generate_proof_as_owner`) are permanently unreachable — the "owner"
   address holds no power it can exercise. No shared BM, no governance
   dependency, no multisig trust.
3. All trading uses `generate_proof_as_trader(&mut bm, &TradeCap, ctx)`,
   which validates the cap (allow-listed ID, matching BM), not the sender —
   so it works for any current curator and survives cap rotation.

One vault ↔ one BalanceManager, serving all pools.

**Phase-2 verification task**: integration test on testnet confirming
`pool::place_limit_order` and settlement work against a **non-shared,
wrapped** BM (pool functions take `&mut BalanceManager` by reference, so this
should be mechanical — but DeepBook has only ever shipped shared BMs, so
prove it). Also monitor DeepBook upgrades for any new shared-BM assumption.

### 6.2 Operations (all curator sessions; no price constraints — decision 6)

- `deposit_to_manager<T>` — session `take` → `deposit_with_cap`.
- `withdraw_from_manager<T>` — `withdraw_with_cap` → session `put`.
- `place_limit_order` / `place_market_order` / `modify_order` /
  `cancel_order` / `cancel_all_orders` — thin pass-throughs via TradeProof.
- `withdraw_settled` — also exposed as a permissionless crank
  (`withdraw_settled_amounts_permissionless` needs no proof).
- Admin-set **pool allowlist** in the adapter (vetted pools only). This is
  registry-style vetting, not an oracle guardrail; it is the remaining
  structural brake on wash-trading exfiltration and can be loosened by
  governance later without touching vault core.

### 6.3 Valuation

`appraise_deepbook(vault, &mut Appraisal, pools…, price_infos…)`: sums
`balance_manager::balance<T>` (base/quote/DEEP) plus `locked_balance(pool,
&bm)` for resting orders, cross-priced into deposit-asset units via Pyth.
Resting orders are valued at locked cost, not optimistic marks.

## 7. Options adapter

1. **Vault as RFQ writer** (phase 3): session takes funds, opens escrowed
   RFQs via `options_rfq`/`auction` (the covered-call vault's flow); MMs bid
   premium; vault receives `Position` + premium (recipient = vault address →
   `receive_position` sweep). Post-expiry `redeem_position` is a
   permissionless crank. Valuation: oracle intrinsic over the position's
   unexercised range (premium mark-to-market is a later refinement).
2. **Vault as MM collateral** (phase 4, the mm-bot integration): a
   `vault_mm` module implements the standardized `release<T>` interface from
   `options_core::collateral`, backed by vault funds, with a vault-bound
   `QuoteSigner`. The curator's mm-bot signs quotes exactly as today
   (`collateral_package`/`release_module` config), collateral releases from
   the vault, and the resulting `Position`/`Coin<Call>` recipients are
   asserted to be the vault. This is the "run real MM strategies on public
   funds" goal with minimal mm-bot changes.

## 8. Threat model highlights

- **Curator self-dealing through allowed venues** (off-market order filled
  from a personal account). With no price guardrails (decision 6) this is
  accepted and disclosed, Hyperliquid-style. Mitigations: lockups, curator
  floor, pool vetting, full dashboard transparency.
- **NAV games around deposit/fulfillment**: same-tx complete appraisal, Pyth
  staleness/confidence bounds, locked-cost valuation of resting orders,
  fulfillment-time (not request-time) crystallization.
- **Malicious adapter**: requires an AdminCap registry entry; removal is an
  instant kill switch. Same blast radius as AdminCap generally; multisig ops.
- **Share inflation / first depositor**: virtual shares + creator seed.
- **Queue starvation**: force-unwind after `unwind_grace_ms` converts venue
  balances to free liquidity permissionlessly.

## 9. Package & deployment

`contracts/trading-vault/Move.toml`: local deps on `core`, `auction`, `rfq`;
DeepBook + Pyth with testnet/mainnet `dep-replacements` mirroring
`vault/Move.toml`. First-party adapters live as modules in the package;
third-party adapters ship as separate packages later.

Deployment checklist (per deployment-manager conventions):
1. `publish_dep_package(…, "trading-vault", …)` after `vault` in
   `tools/deployment-manager/src/main.rs`.
2. `trading_vault { packageId, upgradeCapId, publishDigest, deployedAt }`
   record in `deployments.json` + `crates/deployments` types + `deploy.rs`.
3. Frontend PTBs (deposit / request / fulfill / curator ops) need gas-station
   templates in `sui-tx` `template.rs` `protocol_templates()`.

## 10. Phasing

1. **Vault core** — vault/stakes/CuratorCap/registry/session/appraisal/queue/
   fees/closure + Move unit tests locking the fee & share math.
2. **DeepBook adapter** — wrapped-BM spike test first, then ops + appraisal.
3. **Options adapter, RFQ-writer mode** + redeem cranks.
4. **`vault_mm` release module** + mm-bot config surface.
5. **Backend & dashboard** — indexer event family (VaultCreated, Deposit,
   WithdrawRequested/Fulfilled with fee breakdown, SessionSettled,
   PositionOpened/Closed, CuratorRotated, VaultClosed), NAV snapshot job
   (devInspect appraisal on cron) for performance series, api-service
   endpoints, keeper cranks (settled sweeps, redeems, queue fulfillment,
   closure distribution).

## 11. Open items

- Testnet verification: non-shared wrapped BalanceManager trades on DeepBook
  pools (phase-2 gate).
- BD (non-blocking): DeepBook app authorization for
  `new_with_custom_owner_caps_v2`, which would also open shared-BM options.
- Premium mark-to-market for options positions (post-MVP).
- Covered-call vault merge (explicitly out of scope; migration must seed
  cost basis at migration-day NAV — no retroactive perf fee).
