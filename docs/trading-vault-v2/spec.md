# Trading Vault v2 — Normative Capital-Structure Specification

**Terms version: 1** · Spec status: FROZEN for the v2 release ·
Implementation: `contracts/trading-vault-v2` (package `vault_v2`)

This document is the normative product layer required by
[trading-vault-overhaul-plan.md](../trading-vault-overhaul-plan.md) §9.1. Move
source comments explain implementation; they cannot create or change product
economics. Every vault stores `terms_version` and a `spec_hash` of this
document at creation and emits both in `VaultCreated`.

All values are denominated in the vault's accounting asset's smallest units
unless stated. All multiply-before-divide arithmetic uses u256 intermediates
with floor division; exceptions that round UP are called out explicitly.

## 1. Definitions

- **NAV** — the total portfolio value produced by one complete `Appraisal`:
  every free balance (oracle-attested for non-accounting assets), every
  custodied adapter position (valued exactly once by its custodian adapter),
  and, while external exposure is live, the attested external-account equity.
  An appraisal is invalidated by any same-transaction balance mutation
  (`mutation_seq`), position change, asset-set change, or capital mutation
  (`capital_seq`).
- **Senior claim `C`** — the senior capital account: senior principal plus
  accrued hurdle (§2). Not an amount of assets; a priority of claim.
- **Senior principal basis** — senior deposits net of pro-rata reductions at
  exits, without hurdle. Reference value for the CappedParticipating total
  return cap.
- **Senior NAV / junior NAV** — the §3 waterfall's allocation of NAV.
- **Coverage breach** — tranched vault with
  `junior_nav / NAV < maintenance_junior_bps / 10⁴` (and senior shares > 0)
  while not impaired.
- **Impairment** — `NAV < C` with senior shares outstanding. Implies
  `junior_nav == 0` in every upside mode.
- **Active junior generation** — the only junior share generation
  participating in junior NAV. Bumped exactly by reset execution (§6).
- **Wiped position/claim** — a junior position or queued request whose
  generation is below the active generation. Permanently zero-value.
- **Terminal insolvency** — at settlement snapshot, `NAV < C`: senior takes
  all remaining assets pro rata; junior entitlement is zero.
- **Risk-off** — capital risk state ∈ {CoverageBreach, Impaired, ResetPending}
  or the curator commitment breach flag is set (§7).

## 2. Senior hurdle accrual

Simple, continuously time-weighted, **cumulative** accrual that continues
during impairment (plan §8.2 selected rule):

```text
elapsed  = min(now − last_accrual_ms, ACCRUAL_CAP_MS)
accrual  = C × senior_hurdle_bps_annual × elapsed / 10⁴ / MS_PER_YEAR
C        = C + accrual        (floor division, u256 intermediates)
```

- Time basis: `MS_PER_YEAR = 31_536_000_000` (365 days).
- `ACCRUAL_CAP_MS = 63_072_000_000` (2 years) is an **overflow sanity bound
  only**, never an economic pause. Keeper cadence obligation: consumed
  appraisals must occur at intervals ≪ the cap (operationally: at the existing
  mark-refresh cadence, minutes–hours). An interval beyond the cap silently
  under-accrues; this is an operational failure, not contract behavior.
- Accrual runs at the head of every capital mutation (deposit, fulfillment
  batch, reset propose/execute, external release, capital crank), so no
  capital flow can capture or skip accrual retroactively.
- Accrued hurdle does not itself accrue (no compounding). Rounding: floor,
  dust favors junior.
- Senior deposits add their value to `C` at deposit time (no retroactive
  accrual). Senior fee-share mints add `curator_net` to `C` (§5). Senior
  exits reduce `C` pro rata (§4).

Worked example (normal): C = 1,000,000 at 10% (1000 bps). After exactly one
year C = 1,100,000; after 18 months, 1,155,000 (second half-year accrues on
the grown claim — piecewise-linear on current C, not compounded on principal
alone, and not exponential). Boundary: hurdle 0 or C 0 ⇒ no accrual; clock
regression ⇒ no accrual; 10-year gap ⇒ only the 2-year cap accrues.

## 3. Waterfall (all three senior-upside modes)

Computed on every consumed appraisal AFTER accrual, from
`(NAV, C, senior_principal_basis P)`:

```text
preferred     = min(NAV, C)
residual      = NAV − preferred
participation =
    PreferredOnly          → 0
    CappedParticipating    → min(residual × participation_bps / 10⁴,
                                 max(0, P × total_return_cap_bps / 10⁴ − preferred))
    UncappedParticipating  → residual × participation_bps / 10⁴
senior_nav    = preferred + participation
junior_nav    = NAV − senior_nav
```

Invariants (per mode; tested in `capital_tests.move`):
- All modes: `senior_nav + junior_nav == NAV` exactly; junior absorbs every
  loss until `junior_nav == 0`; `participation ≤ residual`.
- PreferredOnly: `senior_nav ≤ C` and `senior_nav ≤ NAV`.
- CappedParticipating: `senior_nav ≤ min(NAV, preferred + capped participation)`;
  the cap binds on total senior return relative to principal basis.
- UncappedParticipating: junior retains exactly `(10⁴ − participation_bps)`
  of residual.

Worked examples: NAV 1,000,000, C 400,000, P 400,000 —
PreferredOnly ⇒ (400,000 / 600,000). Uncapped 30% ⇒ (580,000 / 420,000).
Capped 50% with 120% cap and C accrued to 410,000 ⇒ participation
min(295,000, 480,000−410,000)=70,000 ⇒ (480,000 / 520,000).
Boundary: NAV 0 ⇒ (0/0). NAV < C ⇒ (NAV/0) in every mode.

## 4. Share pricing, deposits, exits

Each tranche keeps an independent supply and prices with the v1 virtual
offset (O = 1,000,000):

```text
shares minted = value × (S_t + O) / (nav_t + 1)
claim value   = shares × (nav_t + 1) / (S_t + O)
```

An untranched vault is the degenerate case: one book (stored in the junior
fields), `Tranche::Untranched`, always Healthy.

**Deposits** (whitelist-gated primary issuance; mints a transferable
`VaultPosition` NFT carrying shares, cost basis, lock expiry, tranche,
generation; each deposit is a new lot with its own lockup):
- Blocked when: vault not Open, protocol/vault paused, Impaired or
  ResetPending (recapitalization only via reset execution), and for senior
  additionally in CoverageBreach.
- Senior deposit requires post-deposit target buffer:
  `junior_nav × 10⁴ ≥ target_junior_bps × (NAV + value)` (junior NAV is
  unchanged by a senior deposit). Consequence: junior seed capital must
  precede the first senior deposit. Senior deposits add `value` to `C` and P.
- Dead-tranche rule: a tranche with outstanding shares and zero tranche NAV
  cannot price deposits (`vault_dead`); junior recapitalization goes through
  the reset (§6).

**Withdrawals**: a request consumes the whole position object into a queue
entry (split first for partial exits); wiped junior positions are rejected
(burn path instead). Queued shares remain outstanding: they keep earning P&L
and, for senior, hurdle accrual until fulfilled. Two FIFO lanes (senior lane
0; junior/untranched lane 1) under one global sequence. Fulfillment pays,
among currently payable lane heads, the lowest global sequence; strict FIFO
within a lane. A junior head is unpayable while the vault is risk-state
blocked (CoverageBreach/Impaired/ResetPending); per-request asset
unavailability and the aged accounting-asset grace fallback are unchanged
from v1. When nothing is blocked this reduces exactly to a single global
FIFO. Force-unwind unlock ages on the oldest head across lanes, so a blocked
junior lane still counts as unmet exit demand.

**Senior claim reduction at fulfillment** (batch-locked book values):

```text
claim_reduction     = C_locked × shares_burned / S_senior_locked
principal_reduction = P × shares_burned / S_senior_locked
```

Claim-per-share is invariant under exits; an impaired exiter's unpaid arrears
are extinguished, never accreted to remaining seniors.

## 5. Performance fees

Exit crystallization only (plan §8.8), per position lot:

```text
value        = shares × (nav_t + 1) / (S_t + O)        (batch-locked ratio)
profit       = max(value − basis, 0)
gross_fee    = profit × curator_fee_bps / 10⁴
protocol_cut = gross_fee × protocol_fee_bps / 10⁴       (share OF the fee)
curator_net  = gross_fee − protocol_cut
payout       = value − gross_fee                         (converted to the
               payout asset at the batch price, exit haircut applied)
```

The protocol cut leaves as cash to the treasury. The curator's net fee is
minted as shares at the same locked ratio of the **same tranche** the fee was
earned in, credited to the escrowed curator commitment position — PPS-neutral
for remaining holders. A senior fee mint additionally credits `curator_net`
to `C` (and P) in the same batch; without that credit new senior shares would
dilute the existing claim. Fee mints are exempt from the senior target-buffer
issuance gate. Fee shares carry no fresh lockup; their basis equals
`curator_net`.

Basis rules: split allocates basis pro rata (floor; parent keeps remainder);
merge adds shares and basis exactly and takes `max` of locks; secondary
transfer never changes basis, lock, tranche, or generation — a buyer inherits
the embedded fee liability. Loss recovery below original basis is never
charged (per-lot high-water effect).

At terminal settlement (§8) fees crystallize identically per redemption, but
the curator's net accrues as a cash claim on the pool (`curator_fees_accrued`,
claimable by the current cap) because share mints are impossible after the
snapshot.

## 6. Junior generational reset (exact v1 rules)

Eligibility (objective, from a complete appraisal): active junior shares > 0,
`junior_nav == 0`, `NAV < C`. Detection stamps `impaired_since_ms` and enters
risk-off.

1. **Proposal** (permissionless): records the appraised terms and
   `executable_at_ms = max(impaired_since + 7d, proposed_at + 7d)`
   (`RESET_SEASONING_MS = 604,800,000`, immutable protocol minimum for both
   seasoning and notice). Emits `JuniorResetProposed`. State → ResetPending.
2. **Cancellation**: ANY consumed appraisal showing `junior_nav > 0` cancels
   the proposal and clears `impaired_since_ms` (emits
   `JuniorResetCancelled`). Time alone can never execute a wipe.
3. **Execution** (permissionless for any issuance-whitelisted sender, no
   earlier than `executable_at_ms`): consumes a FRESH appraisal that must
   still prove eligibility, and in the same transaction supplies the
   recapitalization deposit `D` in the accounting asset. `N` and `C` are
   re-derived from the execution appraisal; recorded proposal terms are
   disclosure only. Minimum deposit, rounded UP:

   ```text
   D ≥ (C − (1 − t)·N) / (1 − t),  t = target_junior_bps / 10⁴
   and N + D > C
   ```

   which guarantees `post_junior_nav = N + D − C > 0` and
   `(N + D − C)/(N + D) ≥ t`. The first `C − N` units of D cure the senior
   deficit rather than becoming junior NAV.
4. **Generation transition**: increment the active generation, set its supply
   to the genesis mint `D × O` for the recapitalizer (standard offset math on
   a zero book — the recapitalizer owns the entire generation, worth exactly
   `N + D − C`). The senior claim is NOT written down. State recomputes to
   Healthy.
5. **Old generation**: permanently zero. Old positions/requests are `Wiped`:
   requests fulfill/settle at zero; wallet NFTs are burnable via
   `burn_wiped_position`; the old escrowed curator commitment is burned and
   replaced on the next commitment deposit.
6. **Curator participation**: the reset sets the commitment-breach flag; risk-
   increasing activity stays disabled until the curator funds a compliant
   NEW-generation commitment. The reset cannot waive that commitment.
7. **No discretionary seizure**: no admin or curator early-wipe, quote change,
   generation revival, or NFT confiscation paths exist.

Worked example: N = 700,000, C = 751,438 (750,000 + 7 days accrual at 10%),
t = 20% ⇒ D ≥ (751,438 − 560,000)/0.8 = 239,298 (ceil). Post: junior NAV =
700,000 + 239,298 − 751,438 = 187,860; buffer = 187,860/939,298 = 20.0%.

## 7. Capital risk states and the action matrix

States: `Healthy`, `CoverageBreach`, `Impaired`, `ResetPending` — re-derived
at every consumed-appraisal mutation and by the permissionless
`crank_capital`. Orthogonal lifecycle: `Open`, `Closing`, `Closed`
(+ settled). The curator-commitment breach flag (§8.6 of the plan: marked
value of the escrowed commitment < `min_curator_commitment_bps` of NAV, while
Open and enforcement is on; also set pessimistically by rotation and reset
until re-tested) applies the same gate set minus the junior-lane block.

Mechanical rule: **in risk-off, nothing leaves free balances except through
withdrawal fulfillment** — deployment stops, unwinding continues.

| Entry point | Healthy | CoverageBreach | Impaired | ResetPending | Closing | Closed |
| --- | --- | --- | --- | --- | --- | --- |
| deposit (senior) | ✓ (target gate) | ✗ 135 | ✗ 135 | ✗ 135 | ✗ 72 | ✗ 72 |
| deposit (junior/untranched) | ✓ | ✓ | ✗ 135/97 | ✗ 135 | ✗ 72 | ✗ 72 |
| deposit_into_commitment | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ |
| execute_junior_reset | n/a | n/a | via proposal | ✓ (after deadline) | ✗ | ✗ |
| request_withdraw | ✓ | ✓ (junior queues too) | ✓ | ✓ | ✓ | ✗ 136 (pool) |
| fulfillment: senior lane | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ (pool) |
| fulfillment: junior lane | ✓ | ✗ blocked | ✗ blocked | ✗ blocked | state-dependent | ✗ (pool) |
| begin_session (curator) | ✓ take-capable | forced (take ✗ 91) | forced | forced | per risk state | ✗ |
| begin_quote_session | ✓ | ✗ 124 | ✗ 124 | ✗ 124 | ✗ 72 | ✗ 72 |
| vault_mm release (release_for_mm) | ✓ (if opted in) | ✗ 124 | ✗ 124 | ✗ 124 | ✗ 124* | ✗ |
| release_external | ✓ (budget+rate) | ✗ 124 | ✗ 124 | ✗ 124 | ✗ 72 | ✗ 72 |
| begin_force_session / begin_crank_session | ✓ (unlock rules) | ✓ | ✓ | ✓ | ✓ | ✓ |
| return_external / receive_* / repayments | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| appraisals (begin/legs/crank) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| release_commitment | ✓ (floor) | ✗ 124 | ✗ 124 | ✗ 124 | ✓ (no floor) | settled path |

*vault_mm release additionally requires `mm_release_enabled` and Open-state
quote flows; risk-off is checked inside `release_for_mm` itself.

Coverage thresholds: creator-selected `target_junior_bps` (≥ protocol
minimum) gates new senior issuance; immutable `maintenance_junior_bps`
(≥ protocol minimum, ≤ target) triggers CoverageBreach. Breach blocks
junior-lane fulfillment; the senior lane keeps flowing; the junior lane
resumes at its own head, in original order, when the breach cures.

## 8. Terminal settlement pool

`Closing` stops deposits; sessions continue for unwind. `finalize_close`
(permissionless) requires zero custodied positions, zero external exposure,
and only the accounting asset remaining ⇒ `Closed`.

`snapshot_settlement` (permissionless, one-time): consumes a final complete
appraisal, runs the waterfall once, and freezes:

```text
senior_pool = senior_nav(final)      senior_supply = outstanding senior shares
junior_pool = NAV − senior_pool      junior_supply = active-generation junior shares
```

Senior settles first — under terminal insolvency senior_pool = NAV and junior
gets zero; pro rata within each tranche. Outstanding queued requests settle
from the pool at the snapshot entitlement (`entitlement = pool × shares /
supply`, floor), permissionlessly and in any order (NAV is frozen). Any
position holder may redeem directly against the pool at any later time — no
queue, no appraisal, no keeper; unredeemed positions are perpetual claims and
the vault persists as a claim-only shell. "Fully closed" means **settled**,
not zero outstanding shares. Wiped generations redeem at zero. Fees per §5.
Escrowed curator commitments become ordinary pool claims via
`withdraw_commitment_settled`.

Payout-asset preferences do not outrank capital priority: the settlement
snapshot is denominated solely in the accounting asset (`finalize_close`
already requires all other assets gone).

## 9. Boundary examples (normative test anchors)

| Case | Anchor test |
| --- | --- |
| Zero-supply genesis pricing (offset math) | `vault_tests::deposit_mints_transferable_position_with_lot_metadata` |
| Split/merge conservation, lock max | `vault_tests::split_and_merge_conserve_shares_and_basis` |
| Exit crystallization + same-tranche fee mint + claim credit | `tranche_tests::hurdle_accrues_and_senior_exit_takes_claim_with_pro_rata_reduction` |
| Junior wipeout ⇒ impairment ⇒ seasoned, funded reset | `tranche_tests::impairment_reset_end_to_end` |
| Recovery before reset cancels the proposal | `tranche_tests::recovery_cancels_reset_proposal` |
| Blocked junior head never stalls senior | `tranche_tests::coverage_breach_blocks_junior_lane_but_senior_keeps_flowing` |
| Reset minimum-deposit rounding at the target boundary | `capital_tests::min_reset_deposit_cures_and_restores_buffer` |
| Underfunded closure settles senior first, junior zero | `tranche_tests::settlement_is_senior_first_under_shortfall` |
| Post-settlement redemption (queued + wallet + escrow + fees) | `vault_tests::terminal_settlement_pool_end_to_end` |
| Accrual overflow cap | `capital_tests::accrual_elapsed_capped` |
