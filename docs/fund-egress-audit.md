# Fund-egress audit

**Audited at `bfc5dbb`, 2026-07-31.** Scope and unread regions are stated at the
end; read that section before treating any row here as complete.

## The question this answers

> "We just NEED to make sure that we can't lock and lose money forever."

That is not the same requirement as "no bugs", and only this one is testable. So
for every place customer money can come to rest, this document records three
things: **where it sits, how it gets out, and what has to be true for that to
work.** A row whose exit has a precondition that cannot be satisfied during the
failure it is meant to survive is a blocker, and is labelled one.

## Summary

| Where funds sit | Exit | Verdict |
|---|---|---|
| `mm-collateral` | `withdraw<T>` | **clean** — owner + balance, nothing else |
| core buckets | `exercise`, `redeem_position`, `burn_expired_option` | **clean** — no oracle, no cap |
| CCTP in flight | Circle receive PTB | **clean** — permissionless; the relay only pays gas |
| `trading-vault` | `request_withdraw` → `fulfill_withdrawals` | **conditional, recoverable** — needs a fresh price, but the bound is adjustable on the spot |
| `vault` (user vault) | `complete_withdraw` | **BLOCKER** — needs a fresh price, and the lever that relaxes that is unreachable during the outage |

Four of five surfaces either have no external dependency or have a working
lever. One does not.

---

## Clean rows

### mm-collateral

```
mm_collateral::withdraw<T>   assert sender == account.owner
                             assert balance >= amount
```

No oracle, no capability, no phase, no off-chain service. Unconditional.

### Core buckets

```
bucket::exercise              :458
bucket::redeem_position       :504
bucket::burn_expired_option   :555
```

None takes a `PriceInfoObject`; none requires a capability.

**`invalidate` is ingress-only.** The `!bucket.invalidated` assertion appears at
`:214 :343 :406 :729` — all write and flow paths. It appears on none of the three
exits above. An invalidated bucket still lets holders exercise and redeem, which
is the same shape as the pause surface: it stops new exposure without trapping
existing exposure.

### CCTP bridge

The relay is a **fee-payer and submitter, not a gatekeeper.** The mint is
Circle's standard five-call receive PTB (`cctp-relay/src/sui_mint.rs:80-112`),
driven by a message plus an attestation retrieved from Circle's public Iris API
(`iris.rs`). The recipient is encoded in the burn, not in the submitter.

So a relay outage is a **liveness** problem and never a custody one — anyone
holding the message and attestation can submit the same transaction. This is
worth recording explicitly because the 2026-07-30 bridge outage looked worse than
it was for exactly this reason.

---

## Conditional but recoverable: trading-vault

`fulfill_withdrawals` requires a complete `Appraisal` — every asset type valued,
every position appraised, no external exposure pending — and appraisal requires
`PriceAttestation`s fresh within `registry::max_price_age_ms`.

On paper this is the same shape as the user vault: payouts gated behind price
freshness. **The difference is that the bound can be moved.**

```
trading-vault/sources/registry.move:124
  public fun set_max_price_age_ms(_: &AdminCap, cfg, ms)   -> applies IMMEDIATELY
```

It is not an exception. Every AdminCap setter in that registry applies at once —
`:81 :86 :91 :96 :101 :107 :112 :118 :124 :130 :137`. None is deferred, none is
phase-gated. Under a degraded feed, an admin widens the bound, appraisal
proceeds, and the queue drains. There is no wedge.

---

## BLOCKER: the user vault can strand live capital

### The exit and its preconditions

```
complete_withdraw          requires pps.contains(round)
  └── requires finalize_round, which requires ALL of:
        · Pyth fresh on BOTH legs: now − publish_time <= max_price_age_secs
                                                       (vault/sources/oracle.move:100)
        · conf/price <= max_conf_bps                   (oracle.move:111-113)
        · positions_head == positions_tail             (all positions redeemed)
        · open_rfqs == 0 && open_swap_rfqs == 0
        · proceeds swapped, unless hold_premium_in_settlement
```

`finalize_round` is permissionless — anyone may crank it. That is the good half.
It cannot run at all without a live Pyth cross, and `complete_withdraw` cannot
pay out until it has.

### Why the escape hatch does not open

`update_oracle_feeds` (AdminCap) exists for precisely this emergency and its own
doc says so. It asserts `phase == Settling` (`vault.move:358`).

`phase` is a private field of `Vault`, so the enumeration below is exhaustive
rather than merely thorough — no other module can write it:

```
vault.move:750    phase = Active      end of finalize_round_internal
vault.move:1047   phase = Settling    inside maybe_enter_settling

maybe_enter_settling call sites — exactly two:
  :472   crank_redeem              then asserts positions_head < positions_tail
  :661   finalize_round_internal   caller resolved the oracle first
```

In a round where nothing sold, `positions_head == positions_tail`, so
`crank_redeem` aborts — **and a Move abort reverts the phase write with it.** The
only other path needs the oracle. So `phase` stays `Active` and
`update_oracle_feeds` is unreachable in exactly the situation it was written for.

### The liveness that was written for this case and does not survive

`maybe_enter_settling` names the case explicitly:

> *"Rounds that never selected a bucket (`current_expiry_ms == 0`) settle
> immediately — liveness for zero-deposit / zero-bid rounds and genesis."*

`finalize_round_internal:752` resets `current_expiry_ms = 0`, so `now >= 0` is
trivially true and the phase write does fire — and is then discarded by
`crank_redeem`'s positions assert in the same transaction. Three individually
correct pieces of code, in order, producing a state none of them intended.

### The one escape, and why it cannot be manufactured

`settle_rfq` takes **no `PriceInfoObject` and asserts no phase** (body checked in
full, `:939-1008`), and it is the only writer of `positions_tail` besides the
reset. So an *already-open auction with a winning bid* can be settled during an
outage, creating a position, which lets `crank_redeem` succeed and persist
`Settling`.

But it cannot be created during the outage:

```
select_bucket(..., underlying_info, settlement_info: &PriceInfoObject, ...)
open_rfq     (..., underlying_info, settlement_info: &PriceInfoObject, ...)
```

Both take the oracle. The round cannot be started and no RFQ can be opened while
the feed is down.

### The wedged state

```
phase persisted Active · current_expiry_ms == 0 (post-finalize, pre-select)
Pyth dead · no won auction outstanding

select_bucket        needs oracle       -> cannot start the round
open_rfq             needs oracle       -> cannot create the escape
crank_redeem         needs a position   -> aborts, phase write reverts
finalize_round       needs oracle       -> aborts
update_oracle_feeds  needs Settling     -> unreachable
```

`complete_withdraw` pays from `withdrawal_pool` only, so in this state **capital
in `deployable` has no exit at all.** Already-finalized rounds still pay out and
`instant_withdraw_pending` still works for pending deposits — it is not total,
but live capital is stuck.

### Why the obvious config lever does not help

`update_config` is documented as taking effect *"at the next `finalize_round` so
rules can't change mid-flight."* If a stale oracle blocks finalize, a queued
config change never lands. **The mechanism that would widen the staleness bound
is gated behind the operation that bound is blocking.**

And the bound itself is immutable: `max_price_age_secs` and `max_conf_bps` are
set at `create_vault` and have no setter. The only two lines in `vault.move` that
mutate `vault.config` are both in `update_oracle_feeds`, and both write feed
*ids*.

### Recovery status

```
permissionless   none
AdminCap         none — update_oracle_feeds unreachable
UpgradeCap       NOT IMPLEMENTED at any layer we control
republish        does not migrate — new ids, old vault orphaned with the money in it
```

On the upgrade path specifically: the caps are retained, but nothing spends them.
`move-publish` records `upgrade_cap_id` and never uses it; no Move-side upgrade
wrapper exists in any of the 85 `.move` files; and our own signing services
reject `Command::Upgrade` by policy in four places. Recovery is possible only by
driving `sui client upgrade` by hand — a procedure with no automation, no
rehearsal, and no record of ever having been performed.

**On-chain state of the caps, verified 2026-07-31** (chain `4c78adac`, testnet):

```
staging   11 of 11 present   version = 1   policy = 0 (COMPATIBLE)
prod       2 of 2  present   version = 1   policy = 0
```

`version = 1` everywhere is direct chain evidence that **no package has ever been
upgraded.** `policy = 0` means none has been made immutable or additive-only, so
an upgrade remains permitted by the objects themselves.

**Custody is unconfirmed.** The owners are addresses named in our own config —
`0xab8d1b5a…` (auth-service `admin_addresses`) holds staging's eleven and prod's
testTokens cap; `0xf2cb38a4…` (the `deployer` in `deployments.json:12`) holds
prod's core cap. An address in config proves we *know* it, not that we can *sign*
for it, and the two environments have different custodians.

### Recommended fix

> **Status as of 2026-07-31: recommendation only. Not approved, not implemented.**
> Nothing below has been built. If you are reading this and the code exists,
> someone acted after this date — check the history rather than assuming this
> document tracked it.

`force_settle(&AdminCap, vault, clock)` — no oracle, no position requirement.

**Not permissionless.** In the wedged state `current_expiry_ms == 0`, so a
permissionless predicate is trivially true from the moment finalize completes,
and `select_bucket_internal:816` asserts `phase == Active` — which would let
anyone flip a freshly-finalized vault to `Settling` and race every round start.
The guard that removes the griefing (`current_expiry_ms > 0`) also removes the
escape, because `current_expiry_ms == 0` *is* the wedged state. AdminCap costs
nothing real, because the function it unlocks is already AdminCap-only.

This is not a new mechanism. It makes `vault` consistent with `trading-vault`,
which already has eleven immediate AdminCap setters on the same money path.

---

## Scope

**Audited:** `contracts/vault`, `contracts/trading-vault`,
`contracts/mm-collateral`, `contracts/core` (bucket + put_bucket egress paths),
and `rust-backend/services/cctp-relay`. Every `paused` call site across all
`*.move` under `contracts/`. `deployments.json` and the on-chain `UpgradeCap`
objects for staging and prod.

**Not audited — no claim is made about any of these:**

- **`vault_dead()`** (`trading-vault/sources/vault.move`) — the guard was read;
  the state space that reaches it was not. There may be a stranding case behind
  it on a row otherwise cleared.
- **The oracle adapters** — `equity-oracle`, `oracle-pyth` (and `dbm-oracle`,
  since removed by SO-334). The
  attestation *consumer* was audited, not the producers, and they are upstream of
  both vaults.
- **Circle's on-chain CCTP Move contracts.** The CCTP row's conclusion that the
  mint does not check the caller is inferred from the standard flow and the
  relay's own description of it, not verified against the contract. It is the one
  row resting on an external system behaving as documented.
- The `auction`, `rfq`, `options-adapter` and `deepbook-adapter` packages.

## Method note

Two claims in this audit were initially wrong in ways worth recording, because
both were caught the same way — by checking a claim against its evidence rather
than against memory of it.

1. *"Recovery is a package upgrade."* True that the cap is retained; false that
   anything can spend it. Retention is not exercise.
2. A cap query returned `notExists` for all eleven staging caps — the
   stop-everything result. The query had gone to a **mainnet** RPC
   (`sui-rpc.publicnode.com`, chain `35834a8a`) for **testnet** objects. Checking
   `sui_getChainIdentifier` before reporting is what prevented a false "the
   upgrade keys are gone."

The second is a live hazard for anyone repeating this work: since Sui deprecated
JSON-RPC on public fullnodes, `fullnode.testnet.sui.io` refuses these calls, and
the nearest working substitute is one word away from the mainnet host.
**Verify the chain identifier before trusting an object query.**
