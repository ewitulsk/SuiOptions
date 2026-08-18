# Trading vault evolution: tokenized positions and optional tranches

Revision 2. See the revision history at the end of this document for what
changed from the initial draft.

## Executive recommendation

The current vault is a single capital pool with non-transferable, address- or
curator-cap-keyed ledger shares. It already has the right strategy custody
boundary: a curator may deploy assets through audited adapters, but cannot
withdraw them, and every user entry or exit consumes a complete, atomic NAV
appraisal.

The recommended path is:

1. Replace each ledger `Stake` with a transferable **position object** (an NFT-like
   Sui object) containing shares, remaining cost basis, and lock expiry. Do not
   begin with a fungible coin: cost basis and lock state belong to a particular
   tax lot and cannot follow interchangeable coins safely.
2. Add split and merge operations so a position object is economically fungible
   even though its metadata is lot-specific. Withdrawals consume or escrow a
   position (or a split child), rather than debiting `stakes[address]`.
3. Mint the object under Sui's native object/NFT conventions (`key + store`,
   `Display` metadata, and optional Kiosk support). The whitelist gates vault
   creation and primary issuance only; once minted, **every wallet-held position
   NFT is freely and permissionlessly transferable — no exceptions**. The
   curator's first-loss commitment is enforced by escrowing that one position
   inside the vault (§2.2, §8.6), never by restricting transfer of an object in
   a wallet.
4. Make `Untranched` versus `SeniorJunior` immutable at vault creation. A
   tranched vault maintains two independent share supplies and position types,
   but one custody portfolio. Deposits and withdrawals price against tranche NAV,
   while adapter sessions and the appraisal system continue to operate on total
   vault assets.
5. Apply a deterministic waterfall to a single appraised vault NAV before every
   deposit/withdrawal batch: senior principal plus accrued hurdle first, then
   junior. Junior absorbs losses first; senior only loses once junior NAV is
   exhausted. Do not promise a guaranteed senior yield.
6. Preserve per-position cost-basis performance fees for the first tokenized
   release. For tranches, charge performance fees separately per position from
   the gain in its own tranche NAV. Introduce a global high-water-mark model only
   if fully fungible `Coin<Share>` tokens are a hard product requirement.
7. Ship the entire scope — tokenized positions, tranches, capital-state machine,
   queue lanes, terminal settlement — as **one v2 release**: one package, one
   complete object layout, one audit, one launch (§7). Sui package upgrades
   cannot add fields to published structs, so the full layout must be in the
   first published version regardless; a single release removes the intermediate
   migration a phased rollout would force.

This is a new package (`vault_v2` types), not an in-place layout change to
existing shared vault objects. Existing vaults can remain on the ledger model
and opt into migration through an explicit, audited claim flow.

## 1. What exists today

### 1.1 State and authority

`TradingVault` is a shared object holding:

- an immutable accounting asset and a curator-managed deposit/payout allowlist;
- one `total_shares` supply and a `Table<StakeKey, Stake>`;
- free balances in dynamic fields and adapter-tagged position objects in dynamic
  object fields;
- a FIFO withdrawal queue;
- optional external-account exposure; and
- one current transferable `CuratorCap`.

The stake key is either `Addr(address)` or `Cap(ID)`. A `Stake` carries `shares`,
`cost_basis`, and `locked_until_ms`. This makes ownership cheap to query, but it
also makes the economic claim non-transferable. The curator stake is cap-keyed so
the skin-in-the-game obligation follows curator-cap rotation.

### 1.2 Share and NAV accounting

All NAV, basis, fee, and external-budget values are denominated in accounting-asset
smallest units. The conversion is:

```text
shares minted = deposit value * (total shares + 1,000,000) / (NAV + 1)
claim value   = shares * (NAV + 1) / (total shares + 1,000,000)
```

The virtual offset makes donation-based inflation uneconomic and floor rounding
favors remaining investors. A complete `Appraisal` includes every free asset,
every custodied position, and live external-account equity. It snapshots the asset
set, position count, and balance mutation sequence, so a same-transaction mutation
invalidates the result.

Deposits are synchronous: the caller supplies a complete appraisal and, for a
non-accounting deposit asset, a fresh price attestation. Withdrawals first move
shares and pro-rata basis into a FIFO request; they keep participating in P&L until
the request is fulfilled at a later appraised batch ratio. Aged requests can fall
back to payment in the accounting asset so an unavailable requested asset cannot
block the queue forever.

### 1.3 Current fee model

At withdrawal fulfillment the vault calculates, per request:

```text
value        = request shares * appraised NAV ratio
profit       = max(value - request basis, 0)
gross fee    = profit * curator fee bps
protocol cut = gross fee * protocol share of curator fee
payout       = value - gross fee
```

The protocol cut leaves as cash. The curator's net fee remains in the vault and is
minted as shares at the same batch ratio. This is price-per-share neutral for the
investors who remain. The design is a per-lot lifetime cost basis, not a periodic
high-water mark: unrealized gains are not charged, and transfer is currently
impossible, so there is no ambiguity about whose basis follows a claim.

### 1.4 Custody and liveness properties to preserve

- Curator sessions may take balances only through protocol-allowlisted adapter
  witnesses and must resolve their hot potato in the same transaction.
- Force/crank sessions cannot take free balances and remain available for unwind
  or deterministic maintenance.
- Adapter-tagged positions must all be appraised exactly once before NAV is
  consumed.
- Closure requires no positions, no live external exposure, and no residual
  non-accounting assets.
- A curator cannot withdraw below the configured share floor while the vault is
  open.

Tokenization and tranching should change investor accounting, not weaken any of
these custody constraints.

## 2. Change one: mint the user's position as a token

### 2.1 Why an object token is the safe first version

There are two materially different meanings of "tokenized shares" on Sui:

| Model | Benefits | Main problem |
| --- | --- | --- |
| `Coin<VaultShare>` | Fully fungible, wallet/DEX/collateral friendly | A coin unit cannot carry a unique cost basis, lock expiry, vault ID, or tranche. Transfers and merges destroy the information needed by the current fee model. A distinct Move coin type per vault also needs dynamic publishing or a factory/type strategy. |
| `VaultPosition` object | Transferable object with shares, basis, lock, vault, and tranche bound together; no package per vault | Not directly fungible on a coin DEX; integrations need object-aware split/merge and custody support. |

Use `VaultPosition` first. It solves the requested ownership transfer without
silently changing fee economics. Split/merge provides partial transfers and
partial withdrawals. A later fungible wrapper can custody position objects and
issue coins only after its fee and lock semantics are explicitly designed.

### 2.2 Proposed data model

```move
public enum Tranche has copy, drop, store {
    Untranched,
    Senior,
    Junior,
}

public struct VaultPosition has key, store {
    id: UID,
    vault_id: ID,
    tranche: Tranche,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
    /// Junior-reset generation (§8.5). Untranched and Senior positions
    /// carry 0; Junior positions carry the generation they were minted
    /// under. Present from the first published version — Sui upgrades
    /// cannot add struct fields later.
    capital_generation: u64,
}
```

`VaultPosition` deliberately omits `copy` and `drop` and has `key + store`, so
wallets, kiosks, multisigs, lending protocols, and wrapper contracts can hold and
transfer it without any module-mediated path. **Transferability is unconditional
for every wallet-held position.** There is no controlled-transfer variant, no
recipient whitelist on transfer, and no "non-transferable flag" on any wallet
object — Sui's `store` ability makes such a flag unenforceable anyway, because
`transfer::public_transfer` bypasses module code. The whitelist applies to
`create_vault`, `deposit`, and other primary mint paths only. It must not be
consulted by split, merge, ordinary transfer, or withdrawal.

A plain custom object is Sui's native NFT primitive; `Display` supplies standard
name/image/description rendering without changing ownership semantics. Kiosk
compatibility is additive rather than mandatory custody: a holder must always be
able to own and transfer the position directly. The position should publish a
stable type and metadata schema, not claim conformance to an ERC-721-style
interface that does not exist on Sui.

The vault no longer needs `StakeKey`, `Stake`, or the `stakes` table for ordinary
investors. It still needs explicit aggregate accounting for the current curator's
commitment, and that is solved by **escrow, not by transfer restriction**:

- The curator's first-loss commitment position is held **inside the vault** as a
  dynamic object field keyed by the current `CuratorCap` ID. It is a normal
  `VaultPosition` by type, but while escrowed it is simply not in anyone's
  wallet, so no transfer rule needs to exist or be policed.
- Curator fee shares minted at fulfillment (§1.3, §3.5) credit this escrowed
  position directly — fulfillment has guaranteed access to it, which a
  wallet-held object could never provide.
- The cap holder may split/withdraw from the escrowed position only down to the
  commitment floor (§8.6), through curator-gated vault entry points.
- On curator rotation, the vault releases the old escrowed position to the
  outgoing holder as an ordinary, **fully transferable** position NFT (its claim
  ticket), and the incoming cap must fund a new escrowed commitment position
  before discretionary sessions resume.

### 2.3 Entry, split, merge, transfer, and exit

#### Deposit

Keep today's appraisal, allowlist, attestation, haircut, and share math. Instead of
adding to `stakes[key]`, mint and return one `VaultPosition`:

```move
public fun deposit<T>(...): VaultPosition
```

Returning the object rather than transferring it internally lets a programmable
transaction immediately deposit it into a kiosk, multisig, wrapper, or lending
protocol. Events need `position_id`, `tranche`, and `capital_generation` in
addition to the existing deposit fields.

Lockup semantics change deliberately: today a top-up refreshes `locked_until_ms`
on the depositor's entire stake; under position objects each deposit mints a new
position with its own lock, and earlier positions keep their original expiry.
This is the intended behavior, and the specification records it as a change
rather than inheriting it silently.

#### Split

`split_position(position, shares, ctx)` creates a child. Basis is allocated with
the current pro-rata convention:

```text
child basis = parent basis * child shares / parent shares
```

The final/full slice takes the remainder so basis is never created or lost. Both
objects retain the same vault, tranche, generation, and lock expiry.

#### Merge

Two positions may merge only when `vault_id`, `tranche`, and
`capital_generation` match. Add shares and basis; use `max(locked_until_ms)` so
merging cannot launder a lock. Destroy the empty object's UID. A merge must not
average or reset basis.

#### Withdrawal

Prefer consuming an entire position object into `request_withdraw`. Partial exits
are "split, then request." Store `position_id`, shares, basis, tranche,
generation, and recipient in `WithdrawRequest`, and stamp every request with a
**global sequence number** from a single vault-wide counter (used by the queue
lanes in §3.6). There are two ownership designs:

1. **Consume at request (selected):** delete the position UID and escrow its
   accounting fields in the queue. This is closest to today's removal from the
   stake ledger and prevents double use.
2. **Escrow the object as a dynamic object field:** easier cancellation semantics,
   but adds storage and object lifecycle complexity.

If withdrawal cancellation is added later, reminting a new position ID from the
escrowed fields is acceptable, but the events and indexer must expose the linkage.
Queued claims still earn P&L until fulfillment, preserving current semantics.

### 2.4 Fee implications of transferability

An object position makes the current fee calculation coherent because basis moves
with the token. The sale price paid between users is off-vault and should **not**
rewrite the vault's fee basis. The buyer acquires the seller's embedded fee
liability, exactly as a secondary buyer of an accrued-interest or tax-lot claim
does. The UI must display both current NAV value and embedded basis before a sale.

This model still has three details to decide:

1. **Basis after curator fees.** Today curator fee shares are given basis equal to
   the retained curator fee. Preserve that behavior in the escrowed curator
   position.
2. **Loss recovery.** Today a position that falls below basis and later recovers is
   charged only above its original remaining basis. That is already a per-position
   high-water effect; transfer should not reset it.
3. **Position merge.** Adding bases is correct. Weighted-average basis is the same
   arithmetic result, but store the total integer basis to avoid rounding drift.

A fully fungible coin requires a different model. Viable alternatives are:

- **Vault/tranche high-water mark:** periodically crystallize fees globally when
  PPS exceeds the previous post-fee high-water mark. Mint curator shares using the
  dilution-correct formula. This is compatible with fungibility but charges all
  holders alike and needs a reliable crystallization cadence.
- **Tokenized lots plus fungible wrapper:** the wrapper owns position objects and
  exposes fungible coins; wrapper entry/exit crystallizes embedded fees. This moves
  complexity and liquidity fragmentation into the wrapper.
- **Series tokens:** mint a fungible coin per deposit epoch/basis series. This
  preserves basis approximately but fragments liquidity and creates unbounded
  series bookkeeping.

Do not put a mutable "basis per wallet" beside a fungible coin. Ordinary coin
transfers will bypass that ledger and make fees exploitable.

### 2.5 Contract surface affected

- `vault.move`: replace stake storage and user/cap-keyed deposit/withdraw methods;
  add position mint, curator-commitment escrow (mint, top-up, floor-checked
  release, rotation handover), tranche-lane queue orchestration, and the
  risk-state gates of §8.4b.
- `vault_position.move`: position object, split/merge, vault/tranche/generation
  binding, `Display`.
- `vault_mm.move`: gate the `release_for_mm` outflow and quote-session opt-in on
  the capital risk states (§8.4b).
- `events.move`: position lifecycle events and
  `position_id`/`tranche`/`capital_generation`/`global_seq` fields on deposits,
  withdrawal requests, and fulfillment.
- `errors.move`: wrong position vault/tranche/generation, incompatible merge,
  curator-commitment floor, tranche-lane and risk-state failures.
- Read APIs/indexer: index position ownership from object changes, not
  `stake_of(address)`; calculate displayed position NAV from tranche PPS.
- Keeper/API/frontend: build split + request PTBs, show lock/basis/embedded fee,
  and follow position IDs across queue events.

## 3. Change two: optional senior/junior tranching

### 3.1 Product definition

Tranching is not two investment strategies. It is two claims over the same vault
portfolio with a contractual loss/return waterfall:

- **Senior:** paid first up to principal plus a configured hurdle. It has lower
  downside until junior capital is exhausted and capped or subordinated upside.
- **Junior:** absorbs first loss and receives residual upside after the senior
  claim. It is leveraged to portfolio performance.

"Hurdle," "target," or "priority return" is accurate. "Guaranteed yield" is not:
if total assets are below the senior claim after junior is wiped, senior loses.

### 3.2 Immutable creation configuration

Add an immutable mode to `VaultConfig`:

```move
public enum SeniorUpside has copy, drop, store {
    PreferredOnly,
    CappedParticipating {
        residual_participation_bps: u64,
        total_return_cap_bps: u64,
    },
    UncappedParticipating {
        residual_participation_bps: u64,
    },
}

public enum CapitalStructure has copy, drop, store {
    Untranched,
    SeniorJunior {
        senior_hurdle_bps_annual: u64,
        target_junior_bps: u64,
        maintenance_junior_bps: u64,
        upside: SeniorUpside,
    },
}
```

The choice must be fixed at creation. Turning tranches on after assets exist would
require deciding which holders receive priority and would be an economic exchange,
not a safe configuration update. Hurdle changes should also be immutable for an
existing vault, or apply only to a new epoch after every existing position has an
explicit opt-in conversion.

`target_junior_bps` gates **new senior deposits** using post-deposit tranche values;
`maintenance_junior_bps` is the lower risk-off trigger. This prevents a vault from
advertising protection while having negligible first-loss capital without making
normal mark volatility toggle the state at one boundary. Protocol config caps the
hurdle and participation values and floors both junior thresholds; creators may
choose stricter values.

Because a published Sui struct's layout can never gain fields in an upgrade, the
v2 `TradingVault` carries the full `CapitalStructure` enum and a `TrancheBook`
from its first published version. An `Untranched` vault simply holds the
degenerate book (§3.3).

### 3.3 State additions

Replace the single supply with:

```move
public struct TrancheBook has store {
    senior_shares: u128,
    junior_shares: u128,
    senior_claim: u128,
    last_accrual_ms: u64,
    active_junior_generation: u64,
    impaired_since_ms: Option<u64>,
    reset_proposal: Option<JuniorResetProposal>,
}
```

An untranched vault can use only `junior_shares` internally (or retain a separate
single book) and expose `Tranche::Untranched`. A tranched vault uses both supplies.
`JuniorResetProposal` records the old generation, eligibility snapshot, notice
deadline, required recapitalization, and terms needed by the two-stage flow in §8.5.

`senior_claim` is the senior capital account, denominated in accounting units. It
accrues the hurdle with checked `u256` intermediates:

```text
elapsed       = min(now - last_accrual, configured accrual cap)
hurdle accrual = senior_claim * annual bps * elapsed
                 / 10,000 / milliseconds_per_year
accrued claim = senior_claim + hurdle accrual
senior NAV    = per the mode waterfall in §3.4a
junior NAV    = total appraised NAV - senior NAV
```

The elapsed-time cap is an overflow sanity bound only, sized generously (at least
one year) so it cannot plausibly bind in operation; the keeper runbook (§9.4)
must schedule appraisal cranks well inside it, because an interval beyond the cap
silently under-accrues the senior hurdle.

On a senior deposit, add the deposit value to `senior_claim`. On senior
withdrawal **fulfillment** (not at request time — queued shares stay outstanding
and keep participating in accrual and P&L until paid), reduce the claim pro rata
by the shares burned:

```text
claim reduction = accrued senior_claim * shares_burned / senior_supply
```

both taken from the batch-locked book. When senior is healthy this equals the
gross senior value removed; when senior is impaired it exceeds the value paid,
which means an exiting senior holder's unpaid arrears are extinguished rather
than silently accreting to the remaining senior holders — claim-per-share is
invariant under exits by construction. Junior deposits and withdrawals do not
directly change the claim. If senior is impaired, depositing senior at the
impaired senior PPS must not instantly restore old holders at the new depositor's
expense; ordinary senior issuance is paused while `senior NAV < senior_claim`
(§8.5), and recapitalization goes through junior.

### 3.4 Pricing shares in each tranche

Every consumed appraisal first accrues and snapshots the waterfall, yielding:

```text
(senior NAV, junior NAV, senior supply, junior supply)
```

Then use the existing virtual-offset formula independently for each tranche:

```text
senior shares = senior deposit * (senior supply + offset) / (senior NAV + 1)
junior shares = junior deposit * (junior supply + offset) / (junior NAV + 1)
```

Use separate offsets, even if their numeric constants match. A zero-NAV junior
tranche with outstanding shares is dead and cannot accept ordinary deposits; it
needs closure or an explicit recapitalization mechanism. Genesis ordering also
matters: require junior seed capital before the first senior deposit, and enforce
the buffer after each senior entry.

The waterfall must be computed once per deposit or fulfillment batch and locked in
a hot potato, just as `Fulfillment` locks today's NAV/share ratio. Otherwise one
request in a multi-request batch could alter the ratios used by the next request.
For a mixed-tranche batch, snapshot both ratios and update supplies/claim according
to each fulfilled request without recomputing market P&L mid-batch.

#### 3.4a General waterfall (all three upside modes)

One formula covers every mode; `PreferredOnly` is the degenerate case:

```text
preferred    = min(total NAV, accrued senior claim)
residual     = total NAV - preferred
participation =
    PreferredOnly          -> 0
    CappedParticipating    -> min(residual * participation_bps / 10^4,
                                  max(0, senior_principal_basis
                                         * total_return_cap_bps / 10^4
                                         - preferred))
    UncappedParticipating  -> residual * participation_bps / 10^4
senior NAV   = preferred + participation
junior NAV   = total NAV - senior NAV
```

In participating modes senior NAV deliberately exceeds the accrued claim whenever
residual exists; invariants and tests must be qualified by mode (§6).

### 3.5 Fees under tranching

Keep fees subordinate to the waterfall:

1. Appraise gross portfolio NAV.
2. Accrue the senior claim and allocate gross NAV between senior and junior per
   §3.4a.
3. Calculate each withdrawing position's gross value from its tranche ratio.
4. Charge its performance fee only on `max(gross value - position basis, 0)`.
5. Pay the protocol cut in cash and mint the curator's net fee into the **same
   tranche from which it was earned**, at that tranche's locked pre-withdraw
   ratio, credited to the escrowed curator commitment position (§2.2).

Fee-share minting into the senior tranche is treated as a senior deposit for
claim accounting: the mint adds `curator_net` to `senior_claim` in the same
batch. Without that credit, new senior shares would stand against an unchanged
claim and dilute existing senior holders — violating the PPS-neutrality property
the fee mint is designed to preserve. Junior-tranche fee mints do not touch the
claim. Fee mints are exempt from the `target_junior_bps` senior-issuance gate
(§8.4): they are earned compensation crystallized inside a batch, not new outside
capital, and gating them would make senior-side fees unmintable during a breach.

Minting all fees into junior would quietly transfer senior-originated fee value to
junior; minting into senior could consume scarce senior capacity. Same-tranche
minting is the neutral default. Curator floor policy should be specified separately:
require a minimum **junior NAV/share** commitment, not merely a percentage of total
nominal shares, because junior is the first-loss capital the curator should share.

For a later fungible share coin, use one high-water mark per tranche. The senior
hurdle and the fee hurdle must not double count: senior performance fee should
normally apply only above its own investor basis, while hurdle accrual merely
allocates portfolio NAV and is not itself cash income.

### 3.6 Withdrawal liquidity, queue lanes, and insolvency behavior

A single strict-head FIFO across both tranches has a liveness failure, not just a
priority ambiguity: fulfillment stops at the first head it cannot pay, and a
coverage breach makes junior requests contractually unpayable for as long as the
breach lasts. One blocked junior request at the head would therefore freeze every
senior request behind it — exactly when senior holders most need the exit the
tranching exists to protect. There is no time-based fallback for a class block
the way `unwind_grace_ms` unblocks an asset mismatch.

Selected design — **per-tranche FIFO lanes under one global sequence**:

- The vault keeps two FIFO queues, senior and junior (an untranched vault uses
  one). Every request is stamped with the next value of a single vault-wide
  `global_seq` counter at request time.
- Fulfillment rule: among the lane heads that are **currently payable**, pay the
  one with the lowest global sequence; repeat. A lane head is unpayable when its
  tranche is class-blocked (junior during coverage breach or impairment), when
  its payout asset is unavailable (grace fallback unchanged, per request), or
  when free balance cannot fund it.
- Within a lane, order is strictly FIFO — the crank can never reorder holders
  inside a tranche.
- When no class block or asset block is active, this reduces exactly to today's
  single global FIFO, because lowest-global-sequence-first across two lanes is
  the same total order.
- A class-blocked junior lane never stalls the senior lane, and vice versa; the
  blocked lane resumes at its own head, in original order, the moment the block
  lifts.
- The `begin_force_session` age trigger and any "queue head age" logic evaluate
  the **oldest payable head across lanes** for liveness purposes, and the oldest
  head overall for the unwind-pressure signal, so a blocked junior lane still
  counts as unmet exit demand for force-unwind unlock.
- Reserve or enforce a senior liquidity coverage amount before paying junior
  requests while the vault is Closing.
- On `Closed`, the terminal settlement pool of §8.7 replaces lane cranking
  entirely: senior settles before junior, pro rata within each tranche if assets
  are insufficient.

Open-state withdrawals remain liquidity constrained and are not guaranteed. If the
strategy cannot source the requested payout asset, retain amendment and aged
accounting-asset fallback. A fulfillment must never pay more than the request's
tranche value.

### 3.7 Appraisal, adapters, and external accounts

The appraisal engine should continue to produce one total portfolio NAV. Adapters
do not need to know about tranches; they custody and value strategy assets, not
capital claims. External account budget limits should remain a percentage of total
gross NAV, with an optional tighter cap tied to junior NAV if the intended risk
promise is "senior never funds off-chain exposure." That optional rule is more
conservative but can make the strategy unusable after junior drawdowns.

`Appraisal` should snapshot a new `capital_mutation_seq` covering share supplies,
senior claim, and accrual timestamp. Deposit/fulfillment consumption must verify
both asset and capital snapshots so a composed transaction cannot use a stale
waterfall.

## 4. Suggested module decomposition

Avoid doubling the size and audit surface of `vault.move`:

| Module | Responsibility |
| --- | --- |
| `vault.move` | Custody, sessions, lifecycle, balances, queue-lane orchestration, curator escrow |
| `vault_position.move` | Position object, split/merge, vault/tranche/generation binding, `Display` |
| `capital.move` | Capital structure config, senior accrual, waterfall, per-tranche share math, risk-state machine |
| `fees.move` | Basis allocation, per-position crystallization, fee-share mint math (incl. senior-claim credit) |
| `vault_mm.move` | Quote-collateral release path, gated on the capital risk states (§8.4b) |
| `events.move` | Versioned position/tranche/capital events |

Keep arithmetic functions small and pure where Move permits, and test them against a
high-precision reference model. Use `u256` for every multiply-before-divide and
explicitly document which side receives rounding dust.

## 5. Migration and compatibility

Changing the fields of `TradingVault`, `Stake`, and `WithdrawRequest` is not merely
an additive API change. Published Sui package upgrades cannot rewrite the layout of
existing objects as if they were database rows — nor add fields to a published
struct for future instances. Plan for one of:

1. **Parallel v2 vaults (recommended):** deploy `TradingVaultV2`; close/unwind v1;
   users withdraw and deposit into v2. This is simplest to audit.
2. **Migration wrapper:** freeze a v1 vault at a specific appraised NAV, consume each
   ledger stake through a user-authorized migration, and mint an equivalent v2
   position. This needs replay protection, a deadline, queued-withdrawal handling,
   and a source-of-truth snapshot.
3. **Versioned enum/dynamic fields:** only feasible if the currently published
   object layout already has an extension/version hook. The present concrete fields
   do not provide a zero-risk automatic conversion path.

Because v2 ships as a single complete release (§7), its first published layout
already contains every capital-structure field — `CapitalStructure`,
`TrancheBook`, `capital_generation`, the queue lanes, and the settlement pool —
so no second migration boundary exists inside v2 itself. The position's `UID`
additionally allows dynamic-field extension as a last-resort escape hatch for
genuinely unforeseen metadata, but no field consulted by NAV allocation or the
waterfall may live there.

Do not tokenize pending withdrawal requests twice. At the migration boundary either
finish the v1 queue or represent each queued request as exactly one migrated claim.

Off-chain consumers must version event schemas and APIs. At minimum expose:

- vault capital mode and immutable tranche parameters;
- total, senior, and junior NAV/PPS;
- position ID, owner, tranche, generation, shares, basis, lock, estimated gross
  value, and estimated embedded fee;
- senior claim, junior buffer ratio, accrual timestamp, and impairment status; and
- lane and global sequence, position lineage, requested payout asset, and
  settlement status.

## 6. Security invariants and test plan

### Tokenized position invariants

- Sum of live position shares plus queued shares equals the tranche's outstanding
  user shares, including the escrowed curator position (an indexer-verified
  invariant; on-chain state cannot iterate wallet-held objects).
- Split and merge conserve shares and exact total basis; merge cannot shorten a
  lock or cross vault/tranche/generation boundaries.
- A position can be consumed only once and cannot request withdrawal from the wrong
  vault.
- Secondary transfer never resets basis, lock, tranche, or generation.
- The escrowed curator position cannot be released below the required commitment
  floor while it applies; no wallet-held position is ever transfer-restricted.
- Fee-share minting is PPS-neutral up to specified integer dust, including the
  senior-claim credit of §3.5.

### Tranche invariants

- `senior NAV + junior NAV == total appraised NAV` exactly, in every mode.
- **PreferredOnly:** `senior NAV <= accrued senior claim` and
  `senior NAV <= total NAV`; senior cannot gain more than its accrued claim while
  junior exists.
- **CappedParticipating:** `senior NAV <= min(total NAV, preferred + capped
  participation)` per §3.4a; participation is applied only after the preferred
  claim is filled, and the total-return cap binds.
- **UncappedParticipating:** `senior NAV = preferred + participation` per §3.4a;
  junior always retains `(10^4 - participation_bps)` of residual.
- In all modes junior absorbs every loss until junior NAV reaches zero.
- Hurdle accrual is monotonic in time, cannot accrue twice for the same interval,
  and cannot overflow or be accelerated by future timestamps.
- Senior exits reduce the claim pro rata by shares burned (§3.3):
  claim-per-share is invariant under exits, healthy or impaired.
- Deposits cannot cross-subsidize an impaired tranche.
- Waterfall and share ratios are immutable within a fulfillment batch.
- Closure with insufficient assets settles senior before junior and conserves all
  paid value.

### Adversarial and integration tests

- Transfer, split, merge, partial exit, full exit, and transfer-after-lock scenarios.
- Transfer immediately before and after profit/loss; verify embedded fee liability.
- Same-PTB appraisal followed by capital or asset mutation must abort.
- Junior wipeout, senior impairment, recovery, recapitalization, and zero-supply
  genesis cases.
- Reset proposal cancellation on recovery; execution before the seasoning deadline;
  stale/wrong-generation proposals; insufficient recapitalization; atomic funding;
  generation rollover; legacy NFT zero-value cleanup; and ceil-rounded minimum
  deposit at the target-buffer boundary.
- Queue lanes: blocked junior head with senior requests behind it (senior must
  keep flowing); interleaved sequences across lanes reduce to global FIFO when
  unblocked; lane resume order after a breach cures; force-session age trigger
  with only a blocked-lane head outstanding.
- Fee mints into senior during and outside a breach: claim credited, PPS-neutral,
  exempt from the issuance gate.
- Risk-state gating: in `CoverageBreach`/`Impaired`/`ResetPending`, curator
  sessions cannot `take`, quote sessions cannot open, `release_for_mm` and
  `release_external` abort; force/crank sessions and repayments still work
  (§8.4b).
- Terminal settlement pool: snapshot correctness under senior shortfall; direct
  position redemption after `Closed`; queued requests settle from the pool;
  redemption of a stale/wiped-generation position pays zero.
- Non-accounting deposits/payouts with haircuts in each tranche.
- Curator rotation with escrowed first-loss capital and an old claim ticket.
- External-account profit, loss, stale equity, and budget checks under impairment.
- Property/fuzz tests against a Python or Rust rational-arithmetic waterfall model.

## 7. Delivery: one specification, one release

The entire scope ships as a single v2 release — tokenized positions, tranching,
the capital risk-state machine, queue lanes, curator escrow, and terminal
settlement together, in one package with one audit and one launch. Phasing the
contract work (positions first, tranches later) was rejected: Sui cannot add
fields to published structs, so a phased rollout either ships dead capital-
structure fields anyway or forces a second full migration; and a single release
gives the audit one coherent economic model instead of two. The cost is a larger
single audit scope, which the sequence below absorbs by freezing the
specification before implementation.

1. **Publish the economic specification:** before contract implementation, turn
   every decision in §8 into a versioned, non-code product specification with
   formulas, state-transition tables, worked examples, parameter bounds, risk
   disclosures, and governance/change-control rules. Code comments are not the
   source of truth. The required document set is detailed in §9.
2. **Freeze v2 parameters and interfaces:** approve the three senior-upside modes,
   simple cumulative hurdle, target/maintenance coverage tests, queue-lane rules,
   reset rules, curator escrow commitment, senior-first terminal settlement, and
   exit crystallization. Assign a stable version identifier to that approved
   specification. The frozen interface includes the complete object layout —
   every field of `TradingVaultV2`, `TrancheBook`, and `VaultPosition` — since
   none can be added after first publish.
3. **Extract accounting math:** implement `capital.move` and `fees.move` as pure,
   exhaustively tested math against the reference model, before any custody code
   changes.
4. **Implement the full contract surface:** positions (mint/split/merge/
   withdraw), curator escrow, immutable creation config, dual books, linear
   hurdle accrual, three upside modes, waterfall snapshots, two-threshold
   coverage state, risk-state gates across all four outflow paths, queue lanes,
   reset-and-recapitalize flow, and the terminal settlement pool — one codebase,
   no interim deployment.
5. **Integrate off-chain services:** event versioning, keeper PTBs, position pages,
   tranche analytics, breach/impairment/reset alerts, and the disclosures linked
   from every creation and deposit surface.
6. **Audit and shadow:** differential-test NAV/PPS against the normative worked
   examples and reference model, shadow index on testnet, then one audit covering
   custody plus the complete economic surface. Contract audit sign-off must cite
   the exact spec version.
7. **Launch v2 and open migration** from v1 per §5.
8. **Consider fungible wrappers later:** only after integrations demonstrate that
   object positions are insufficient and the global HWM economics are approved.

## 8. Product decisions and TradFi analogues

There is no single "TradFi standard" spanning private credit funds, preferred
equity, securitizations, and structured notes. Each uses seniority differently.
The closest analogue for this vault is a closed pool with preferred and residual
interests: senior has a contractual priority claim and junior owns the residual.
CLO/ABS concepts are useful for coverage tests and payment priority, while private
fund preferred-return concepts are useful for hurdle accrual. Neither should be
copied without adapting its operational machinery to an always-on on-chain vault.

### 8.1 Transfer and token standard — decided

- The whitelist is a primary-issuance control only. It gates vault creation and
  deposit/mint entry points, minimizing the audited ingress surface. Secondary NFT
  transfers are permissionless, for **every wallet-held position without
  exception** — including a released ex-curator claim ticket. The only position
  that cannot be transferred is the escrowed curator commitment, and only because
  it is custodied inside the vault, not because any transfer rule exists.
- The issued claim is a Sui object NFT with `key + store` and standard `Display`
  metadata. It supports direct wallet custody and transfer, and may be listed via
  Kiosk. Split burns no economic value and creates another NFT; merge destroys one
  NFT while conserving shares and basis.
- Performance fees remain attached to the NFT's carried cost basis. A secondary
  buyer receives the embedded fee liability; transfer consideration does not reset
  on-chain basis.
- Compliance consequence, to be stated plainly in the disclosures (§9.3): because
  exits never consult the whitelist, a non-whitelisted party can buy a position
  on the secondary market and redeem it through the queue. The whitelist bounds
  who can *create* exposure, not who can *hold or exit* it. If the whitelist
  exists for regulatory reasons, this is a decision for counsel, recorded in the
  decision records (§9.5), not something the contract can paper over.

### 8.2 Senior hurdle accrual

The hurdle answers: "How quickly does the amount senior is entitled to receive
before junior grow?" It is a claim-allocation rule, not cash mysteriously appearing
in the vault.

#### Option A — simple linear accrual

```text
accrual = reference principal * annual rate * elapsed / year
```

Accrued hurdle does not itself earn more hurdle. Economically, 8% for 18 months is
12%, not `(1.08)^1.5 - 1`. Private preferred-return arrangements often state a
simple annual rate, although the governing documents always control.

**Advantages:** smallest state machine, easy to quote continuously, predictable,
and least sensitive to how often a keeper calls the contract. **Consequences:** it
under-compensates senior relative to compounding over long holding periods and
requires a clear `reference principal` after deposits, withdrawals, and losses.
Use time-weighted principal "lots" or update accumulated claim immediately before
every capital flow so new deposits cannot receive retroactive accrual.

#### Option B — periodically compounded accrual

At each defined interval—daily, monthly, quarterly, or annually—earned hurdle is
added to principal and subsequently earns the hurdle. An 8% annual rate compounded
monthly has a higher effective annual rate than 8% simple.

**TradFi analogue:** compounding is common in debt and some preferred instruments;
the interval and day-count convention are express contractual terms. **Advantages:**
best resembles an accumulating debt-like claim. **Consequences:** senior's claim
grows faster, junior's residual option becomes less valuable, timestamp/rounding
logic expands, and irregular keeper calls must not change the result. The contract
must compute missed periods deterministically rather than "compound once per call."

#### Option C — epoch-based hurdle

The vault defines discrete epochs, such as monthly. At an epoch boundary it freezes
NAV, applies that epoch's hurdle/waterfall, and carries the resulting claims into
the next epoch. Deposits and exits either queue for the boundary or receive an
explicit intra-epoch convention.

**TradFi analogue:** periodic fund NAV, distribution, and securitization payment
dates. **Advantages:** clearest statements and easiest independent reconciliation;
all holders in an epoch share one result. It also creates a natural periodic fee
crystallization point. **Consequences:** less instant liquidity, boundary gaming,
queued capital, and a larger keeper/state machine. It changes today's synchronous
deposit and continuously priced exit model more than the other choices.

#### Does accrual pause during impairment?

"Impaired" means total portfolio NAV has fallen below the accrued senior claim:

```text
impaired <=> total NAV < senior claim
```

Junior NAV is then zero and senior marks below its contractual claim. Continuing
accrual records an ever-growing arrears claim, so all later recovery goes to senior
until those arrears are cured. Pausing (or writing down the claim) stops that debt
overhang and gives junior more recovery upside.

Debt-like instruments commonly continue accruing interest after an economic
shortfall until restructuring/default terms say otherwise; fund preferred returns
and cumulative preferred equity also commonly accumulate, while non-cumulative
preferred returns do not. The contract must choose explicitly.

**Selected rule:** simple, continuously time-weighted, **cumulative** accrual that
continues during impairment, with a maximum elapsed-time sanity cap per calculation
(sized per §3.3 so it is an overflow bound, never an economic pause). It is
deterministic between transactions, needs no epochs, and makes senior priority
real. Display both `senior_claim` and marked `senior_nav` so nobody mistakes
accrued arrears for available assets. If the desired product is less debt-like,
make "non-cumulative while impaired" a creation-time enum—not a curator
switch—and price it as a meaningfully riskier senior product.

### 8.3 Senior upside: capped and uncapped products

Both are possible, but they create different securities:

#### Capped senior (`PreferredOnly`)

Senior receives up to principal plus accrued hurdle; every excess dollar belongs to
junior. This is the clean preferred/residual waterfall and the normal shape for a
debt tranche, preferred equity claim, or securitization tranche. It makes junior's
upside legible and senior's return target easy to quote.

#### Participating senior (`PreferredPlusParticipation`)

Senior first receives its preferred claim, then participates in residual upside.
Participation can be:

- a fixed percentage of residual NAV;
- a fixed participation rate in underlying gains;
- capped at a maximum multiple/return; or
- fully uncapped.

Participating preferred equity exists in private markets, and structured products
frequently combine principal priority with capped or uncapped participation. There
is no universal rate. The consequence is that junior provides first-loss protection
but no longer owns all residual upside; junior should demand better economics or a
smaller required buffer.

**Selected rule:** support three immutable creation-time modes, with
`PreferredOnly` as the default, all evaluated through the single general waterfall
of §3.4a. The participation modes encode an explicit
`senior_residual_participation_bps` and optional `senior_total_return_cap_bps`; do
not use a bare `capped: bool`. Apply participation only after the senior preferred
claim is filled, then split residual NAV deterministically. This yields three clear
products: preferred-only (upside ends at the accrued claim), capped participating,
and uncapped participating. UI naming must distinguish claim accrual from residual
participation, and every invariant, test, and disclosure must state which mode it
applies to (§6).

### 8.4 Junior buffer and senior capacity

The buffer is the equity cushion protecting senior:

```text
junior buffer ratio = junior NAV / total NAV
senior advance rate = senior claim or NAV / eligible portfolio NAV
```

TradFi analogues include overcollateralization (OC), loan-to-value/advance-rate,
capital-ratio, and subordination tests. They are normally tested repeatedly, not
only at issuance, and a failed test restricts distributions or diverts cash toward
senior rather than pretending the breach did not happen.

Available policies are:

1. **Issuance-only minimum.** Require, for example, 20% junior immediately after a
   senior deposit. Simple and deposit-friendly, but market losses can erase the
   protection one block later with no consequence.
2. **Continuous mark test.** Re-evaluate with every consumed appraisal. When below
   threshold, block new senior issuance and junior withdrawals. Stronger, but
   oracle marks can abruptly freeze junior liquidity.
3. **Two thresholds.** A higher issuance threshold and lower maintenance threshold,
   analogous to initial/maintenance margin. This provides operational headroom and
   avoids constant boundary toggling.
4. **Eligible-asset/discounted test.** Haircut risky or illiquid position values
   before calculating coverage. Most protective and closest to credit advance-rate
   practice, but adapter-specific and substantially harder to audit.

**Selected rule:** launch with two thresholds measured on appraised NAV:

- creator selects `target_junior_bps`, subject to a protocol minimum;
- senior deposits require the post-deposit buffer to meet the higher target
  (fee-share mints are exempt, §3.5);
- a lower immutable `maintenance_junior_bps` triggers a coverage-breach state;
- in breach, apply the risk-off gate set of §8.4b, and block junior withdrawals
  (junior-lane fulfillment pauses; the senior lane keeps flowing, §3.6); and
- optionally direct realized distributable gains to rebuilding the buffer before
  junior withdrawals resume.

Do not promise a single universal percentage. It must be calibrated to strategy
drawdown, liquidation horizon, oracle conservatism, and off-chain exposure. A
20–30% junior target might be a product starting point, not a safety conclusion;
historical stress tests should determine the protocol floor.

#### 8.4b Risk-off gating: the mechanical definition

"Blocking risk-increasing activity" must be defined as a concrete set of gated
entry points, because sessions are generic take/put hot potatoes and the vault
cannot inspect what an adapter does with taken funds. The enforceable proxy is:
**in a risk-off state, nothing may leave the vault's free balances except through
a withdrawal fulfillment** — deployment stops, unwinding continues.

In `CoverageBreach`, `Impaired`, and `ResetPending` (and, for the curator-
commitment breach of §8.6, the same set minus junior-lane effects):

| Entry point | Risk-off behavior |
| --- | --- |
| `begin_session` (curator) | Opens, but the session is created with forced semantics: `take` aborts; `put`/`put_position`/`take_position`-for-unwind flows unchanged, so the curator can still flatten positions |
| `begin_quote_session` | Aborts — quote fills draw vault free balances and are permissionless once opted in, so they are deployment by definition |
| `release_for_mm` (`vault_mm`) | Aborts — same reasoning; the `vault_mm` module checks the capital state before any collateral release |
| `release_external` | Aborts — off-chain deployment is risk-increasing per se |
| `begin_force_session` / `begin_crank_session` | Unchanged — put-only by construction |
| `return_external`, `receive_coin`, `receive_position`, repayments | Unchanged — value inbound is always allowed |
| Appraisals | Unchanged — pricing must keep working or nothing can cure |

Every row of this table appears in the state-transition action matrix of §9.1,
and each gate has a dedicated adversarial test (§6).

### 8.5 Impairment, deposits, and recapitalization

There are two related states:

- **Coverage breach:** junior still has positive NAV, but its buffer is below the
  maintenance requirement.
- **Senior impairment:** junior NAV is zero and total NAV is below senior claim.

TradFi equivalents include an OC-test failure, margin deficiency, payment default,
or preferred-equity arrears. Typical remedies are cash-flow diversion, trapping
distributions, deleveraging, new equity, discounted debt exchange, or formal
restructuring.

Accepting ordinary senior deposits while impaired is dangerous. If new money buys
senior shares at marked-down PPS but ranks equally with the old senior claim, it can
dilute either old holders or the new depositor depending on how the claim account is
updated. It can also make an insolvent-looking tranche appear cured without adding
first-loss capital.

**Selected rule:** pause ordinary senior issuance in both coverage breach and
impairment. Permit recapitalization only through:

1. new junior deposits from any issuance-whitelisted user;
2. curator junior deposits, potentially required before risk sessions resume; or
3. a separately specified senior rescue series with its own priority—defer this
   complexity beyond v1.

When junior NAV is zero with old junior shares outstanding, normal virtual-offset
math is not fair. V1 therefore uses the explicit generational reset below. Do not
let the first recapitalizer accidentally donate new value to wiped legacy shares.

#### Exact v1 junior-reset rules

Use **generational junior claims**, not an attempt to find and burn every NFT.
`VaultPosition` carries `capital_generation: u64` from its first published
version (§2.2); the vault stores `active_junior_generation` and the share supply
for that generation. Only the active generation participates in junior NAV. An
NFT from an older generation remains a valid, wallet-visible object but is a
permanently zero-value `Wiped` claim that may be burned through a cleanup
function. It can never become active again merely because NAV later recovers.

A reset is an irreversible economic reorganization, so it is available only through
a two-stage `reset-and-recapitalize` state machine:

1. **Objective eligibility.** A complete appraisal must show all three conditions:
   active junior shares are non-zero, `junior_nav == 0`, and
   `total_nav < accrued_senior_claim`. The vault records `ImpairedSince` and
   automatically enters risk-off mode per §8.4b: no deposits, junior
   withdrawals, or outflows outside fulfillment; unwind, repayments, appraisal,
   and senior withdrawal remain available.
2. **Seasoning period.** Impairment must persist for an immutable protocol minimum
   such as seven days. Initiation records the appraisal values, timestamp, current
   generation, and proposed recapitalization terms. Any complete appraisal showing
   `junior_nav > 0` cancels the proposal and clears `ImpairedSince`. Time alone can
   never execute a wipe.
3. **Advance notice.** Emit `JuniorResetProposed` at initiation with an execution
   time, old generation, senior deficit, required deposit, and post-reset quote.
   Indexer/API/UI alerts must treat this as a critical state transition. A longer
   creator-selected notice may be allowed, but never shorter than the protocol
   minimum.
4. **Atomic revalidation and funding.** Execution consumes a new complete appraisal
   that still proves the eligibility conditions and, in the same programmable
   transaction, supplies a junior recapitalization deposit. There is no standalone
   "wipe" call and no reset without new money.
5. **Minimum recapitalization.** Let `N` be pre-deposit NAV, `C` the accrued senior
   claim, `D` the recapitalization deposit after entry haircuts, and `t` the target
   junior-buffer fraction. Execution requires:

   ```text
   post junior NAV = N + D - C > 0
   (N + D - C) / (N + D) >= t
   ```

   Equivalently, with fixed-point checked arithmetic, require
   `D >= (C - (1 - t) * N) / (1 - t)`, rounded up. This both cures the senior
   deficit and restores the target junior buffer. **`N` and `C` are re-derived
   from the fresh execution appraisal**, not the values recorded at initiation —
   senior exits and accrual during the notice period change both; the proposal's
   recorded terms are disclosure, and the execution-time recomputation is the
   binding requirement. The quote must disclose that the first `C - N` units cure
   senior impairment rather than becoming junior NAV.
6. **Generation transition.** Only after funding is present, increment
   `active_junior_generation`, set its supply to zero, retire the old supply from
   active accounting, and mint genesis junior shares representing exactly
   `post junior NAV` to the recapitalizer. Do not use the old junior denominator or
   let old virtual shares capture the deposit. The senior claim is not written down.
7. **Curator participation.** The recapitalizer may be any issuance-whitelisted
   user, but risk-increasing activity (§8.4b) remains disabled until the curator
   separately satisfies the marked junior commitment in the new generation via
   the escrow of §8.6. The reset itself cannot waive or grandfather that
   commitment.
8. **No discretionary seizure.** Once the objective conditions, delay, fresh
   appraisal, and minimum deposit are satisfied, execution may be permissionless.
   Neither curator nor protocol admin may wipe junior early, change the quoted
   economics, revive an old generation, or confiscate its NFTs.

This rule deliberately gives legacy junior no recovery after execution. Before
execution, legacy junior still owns any recovery above the senior claim and can
benefit if impairment cures during the notice period. The reset exchanges that
future recovery option for fresh capital and a viable vault; the delay, public
quote, and atomic funding requirement make the boundary explicit. If retaining a
legacy recovery warrant is desired, that is a fourth tranche and should be designed
separately rather than hidden inside junior share math.

### 8.6 Curator first-loss commitment

Nominal shares are not a stable floor because senior and junior units have different
PPS and junior shares can be worthless while still numerous.

**Selected rule:** the curator must maintain an **escrowed** active-generation
junior position (§2.2) whose marked value is at least
`min_curator_commitment_bps` of total NAV, subject to an optional protocol
absolute minimum. Test marked value rather than nominal share percentage. The
requirement is checked after creator/curator junior deposits, before new
risk-increasing activity (§8.4b), before any release from the escrowed position,
on curator rotation, and after a junior reset before risk-on state can resume.

During market losses, do not demand that the curator magically top up every block.
Instead, falling below the marked-value threshold applies the §8.4b gate set and
blocks releases from the escrowed position until cured; unwind and user exits
remain available. The escrowed position needs no transfer rule — it is inside the
vault, keyed by the current cap ID, which is the entire enforcement mechanism.
Rotation completes only after the incoming curator funds a compliant
active-generation escrowed position; the old curator's released claim then leaves
escrow as a normally transferable position NFT.

### 8.7 Exit and terminal settlement priority

Yes: **senior should settle first in terminal closure**. Otherwise "senior" is only
a mark allocation and a junior holder can defeat priority by entering FIFO first.
TradFi waterfalls pay senior liabilities before subordinated/residual claims.

Queue and settlement rules:

- While `Open`, the per-tranche lanes of §3.6 apply: strict FIFO within a lane,
  lowest global sequence across payable lane heads, coverage breach pauses only
  the junior lane. Senior priority never means the curator can selectively
  reorder holders within a tranche.
- Once `Closing`, stop new deposits and snapshot/carry forward the waterfall.
  Reserve senior liquidity coverage before paying junior requests. Pay senior
  requests before junior.
- **Terminal settlement pool (replaces both lane cranking and today's
  `enqueue_closed_stake`).** With wallet-held NFTs, no permissionless call can
  force an absent holder into the queue — positions in arbitrary wallets are
  unreachable, and the plan accepts this openly rather than pretending
  otherwise. Instead, `Closed` triggers a one-time, permissionless **settlement
  snapshot**: consume a final complete appraisal, run the waterfall once, and
  freeze each tranche's final per-share entitlement in the accounting asset —
  senior first, pro rata within senior if assets cannot meet all senior claims,
  then junior pro rata, wiped generations at zero.
  - Outstanding queued requests settle from the pool at the snapshot
    entitlement (their positions were already consumed at request time).
  - Any position holder may thereafter redeem **directly against the pool at any
    time** — no queue, no fresh appraisal, no keeper: NAV is frozen, so
    redemption is a pure table lookup and balance split, and late redemption
    costs other holders nothing.
  - Unredeemed positions remain valid perpetual claims; the vault object
    persists indefinitely as a claim-only shell holding exactly the unredeemed
    entitlements. "Fully closed" therefore means *settled*, not *zero
    outstanding shares* — the specification and disclosures must define it that
    way, and the indexer must report unredeemed claim totals per vault.
- Only after senior is paid to its final allocated NAV may junior receive assets,
  pro rata within junior.
- Non-accounting payout preferences must not outrank capital priority. At the grace
  deadline, convert claims to accounting-asset entitlement or require a common
  liquidation basket; the settlement snapshot itself is denominated solely in the
  accounting asset (`finalize_close` already requires all other assets gone).

### 8.8 Fee crystallization — decided for launch

Exit crystallization remains the canonical path. The NFT carries basis, so transfer
does not force a fee event and the holder pays the embedded performance fee when the
position exits.

Periodic crystallization can be added without making the claim fungible:

1. consume a complete appraisal;
2. compute each tranche's post-waterfall PPS;
3. update a tranche-level high-water mark and mint curator/protocol fee shares only
   on gains above it; and
4. record a global fee index so each NFT can identify value already charged when it
   next exits, splits, or merges.

This changes the economics from per-lot exit fees to a pooled high-water-mark model.
A single tranche high-water mark can unfairly charge a new investor for recovery
from losses that occurred before their deposit. TradFi funds address this with
equalization credits, series accounting, or investor-specific loss carryforwards.
The on-chain choices are therefore:

- issue epoch/series NFTs with a shared high-water mark per series;
- give each NFT an equalization credit and `fee_index_snapshot`, while maintaining
  enough aggregate vault state to calculate the fee owed at crystallization; or
- keep exact per-NFT fees lazy and charge only when that NFT exits, splits, or
  merges—which is not truly periodic for an untouched NFT.

Because unrestricted Sui transfer bypasses module code, transfer cannot be relied
on as a lazy-update hook. Exact investor-specific periodic charging cannot iterate
over NFTs held in arbitrary wallets. A pooled high-water-mark/equalization design
must therefore be specified and audited as a second phase rather than presented as
a trivial timer added to the current basis model.

Do not charge both periodic and exit performance fees on the same gain. After a
periodic crystallization, advance the relevant high-water mark/index so exit charges
only subsequent uncrystallized profit. Protocol and curator fees should continue to
be minted at the same locked tranche ratio to preserve PPS neutrality.

### 8.9 Resolved launch profile

Unless product requirements change, v2 launches — as one release (§7) — with:

- permissionless secondary transfer of every wallet-held Sui object NFT position,
  with `Display` metadata; the curator commitment enforced by vault escrow, never
  transfer restriction;
- whitelist-gated primary issuance (with the §8.1 disclosure that exits are not
  whitelist-gated);
- simple, continuously time-weighted, cumulative senior hurdle accrual that
  continues during impairment;
- three immutable senior-upside modes evaluated through the §3.4a general
  waterfall: preferred-only, capped participating, and uncapped participating;
- target plus maintenance junior-buffer tests, with the §8.4b mechanical gate set
  in every risk-off state;
- no ordinary senior deposits during coverage breach or impairment (fee mints
  exempt, with senior-claim credit per §3.5);
- pro-rata-by-shares senior claim reduction at fulfillment (§3.3);
- per-tranche FIFO lanes under one global sequence (§3.6);
- generational, delayed, atomically funded junior reset under the exact rules in
  §8.5, with the minimum deposit recomputed at execution;
- curator commitment held in vault escrow and measured by marked junior value
  relative to total NAV (with a protocol absolute minimum where configured);
- senior-first terminal settlement through a one-time settlement pool, pro rata
  within an underfunded tranche, positions redeemable against the pool forever
  (§8.7); and
- performance fees crystallized on exit.

## 9. Required non-code documentation

All economic and operational decisions must be documented outside the Move source
before implementation is considered ready. The repository documentation is the
normative product layer; source comments explain implementation but cannot silently
create or change product economics.

### 9.1 Normative capital-structure specification

Create a dedicated versioned specification, separate from this exploration, that
contains:

- exact definitions of NAV, senior claim, junior NAV, coverage breach, impairment,
  active generation, and terminal insolvency;
- simple linear hurdle formula, time basis, timestamp rules, cumulative treatment
  during impairment, rounding direction, overflow bounds, and the accrual-cap
  size with its keeper-cadence requirement;
- the §3.4a general waterfall with all three senior-upside modes, including
  participation order and cap application, and the per-mode invariant set;
- the pro-rata senior-claim reduction rule and its fulfillment-time application;
- target and maintenance coverage equations and the action matrix for every vault
  operation in `Healthy`, `CoverageBreach`, `Impaired`, `ResetPending`, `Closing`,
  and `Closed` — including one row per §8.4b outflow path (curator session take,
  quote session, `release_for_mm`, `release_external`);
- the queue-lane rules of §3.6: lane membership, global sequencing, payability,
  and the reduction to global FIFO in the unblocked case;
- the complete junior reset algorithm from §8.5, including minimum-deposit
  rounding, execution-time recomputation, event schema, cancellation,
  old-generation treatment, and worked examples;
- curator escrow commitment, when it is tested, and exactly which actions a
  breach blocks;
- open-state withdrawal priority, the terminal settlement pool, pro-rata shortfall
  allocation, perpetual-claim redemption, and payout-asset conversion rules; and
- exit performance-fee formula, NFT basis inheritance, split/merge allocation, and
  same-tranche curator fee-share minting including the senior-claim credit.

Every formula must include at least one normal case and boundary examples for zero
supply, zero NAV, rounding dust, junior wipeout, senior impairment, recovery before
reset, reset execution, underfunded closure, and post-settlement redemption.

### 9.2 Parameter and governance registry

Publish a table of creator-selectable values, immutable fields, protocol floors and
caps, defaults, units, safe ranges, and who may change each value. A contract upgrade
must not retroactively change immutable vault economics. Any future economic version
gets a new specification version and explicit holder migration/opt-in rules.

At vault creation, store an immutable `terms_version` and a content hash or canonical
URI for the applicable specification and disclosed parameters. Emit both in
`VaultCreated` so indexers and users can recover the terms that governed issuance.

### 9.3 User-facing terms and risk disclosures

Publish concise issuer and holder documentation covering:

- NFT transferability and inherited cost basis/embedded fee liability;
- the fact that secondary transfer and redemption are not whitelist-gated (§8.1);
- hurdle returns as priority claims, not guaranteed yield;
- the meaning and consequences of coverage breach and impairment, including which
  actions each risk state blocks (§8.4b) and that a breach pauses junior — but
  not senior — withdrawals;
- the fact that a completed reset permanently wipes the old junior generation;
- recapitalization value used first to cure the senior deficit;
- senior-first closure, pro-rata loss within an underfunded tranche, and that
  unredeemed positions become perpetual claims on the settlement pool; and
- oracle, adapter, liquidity, external-account, and smart-contract risks.

Vault creation, deposit, purchase UI, and reset alerts must link to the exact terms
version rather than a floating "latest" document.

### 9.4 Operations and incident runbooks

Document monitoring and response procedures for coverage breach, impairment, reset
proposal/recovery/cancellation/execution, stale appraisal, insufficient withdrawal
liquidity, a blocked junior lane, curator-commitment breach, and terminal close
and settlement. Define alert owners, expected keeper calls — including the
appraisal cadence bound required by the hurdle accrual cap (§3.3) — safe retry
behavior, dashboards, and the on-chain evidence required before any public status
communication.

### 9.5 Decision records and release gate

Record separate architecture/product decision records for NFT standard and transfer
policy (including the §8.1 compliance consequence), curator escrow, hurdle
accrual, upside modes, coverage thresholds and the §8.4b gate set, queue lanes,
junior reset, settlement pool, and fee timing. Each record must state
alternatives, the selected rule, rationale, consequences, approvers, date, and
spec version.

The implementation release checklist must fail closed unless:

1. the normative spec and parameter registry are approved and versioned;
2. Move unit/property tests cite specification examples by stable case ID;
3. SDK/indexer/UI behavior is checked against the same state-transition matrix;
4. disclosures and runbooks are published;
5. the audit scope cites the terms version and reports no undocumented economic
   behavior; and
6. deployed package IDs, `terms_version`, specification hash/URI, and audit report
   are recorded in the deployment manifest.

## Revision history

**Revision 2** (this document) — changes from the initial exploration draft,
following design review against the current `vault.move`:

1. **Queue liveness:** replaced the single strict-head FIFO with per-tranche FIFO
   lanes under one global sequence (§3.6), so a class-blocked junior head can
   never starve senior exits; reduces to the old global FIFO when nothing is
   blocked.
2. **Transferability made unconditional:** every wallet-held position NFT is
   freely transferable with no exceptions; the "non-transferable curator NFT"
   (unenforceable against `key + store` anyway) is replaced by holding the
   curator commitment position in vault escrow (§2.2, §8.6), which also gives
   fulfillment a concrete target for fee-share minting.
3. **Single release:** the phased rollout (tokenized untranched first, tranches
   later) is replaced by one complete v2 release with the full object layout,
   one audit, and one launch (§7); `capital_generation` and the tranche book are
   in the first published structs since Sui upgrades cannot add fields.
4. **Senior fee-mint claim credit:** fee shares minted into the senior tranche
   add `curator_net` to `senior_claim` in the same batch, and fee mints are
   exempt from the senior-issuance buffer gate (§3.5).
5. **Senior claim reduction:** specified as pro rata by shares burned, applied at
   fulfillment against the batch-locked book, extinguishing an exiting holder's
   arrears instead of accreting them to remaining seniors (§3.3).
6. **Mode-qualified invariants:** added the §3.4a general waterfall and rewrote
   the tranche invariants per upside mode, removing the PreferredOnly-only claim
   cap from the participating modes (§6).
7. **Risk-off gating defined mechanically:** §8.4b enumerates every gated outflow
   path — curator-session `take`, quote sessions, `release_for_mm`,
   `release_external` — and `vault_mm.move` is added to the module table and the
   action matrix.
8. **Terminal settlement pool:** `Closed` triggers a one-time settlement snapshot
   with senior-first, pro-rata entitlements; positions redeem directly against
   the pool forever, replacing the unreachable `enqueue_closed_stake` sweep, and
   "fully closed" is redefined as settled rather than zero outstanding shares
   (§8.7).

Also folded in from the same review: per-position lockups documented as an
intended semantic change (§2.3), the reset minimum deposit recomputed at
execution (§8.5), the accrual cap sized as a pure overflow bound with a keeper
cadence obligation (§3.3, §9.4), and the whitelist/secondary-market compliance
consequence stated in the disclosures (§8.1, §9.3).
