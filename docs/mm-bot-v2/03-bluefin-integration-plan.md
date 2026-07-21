# Bluefin Pro hedge integration — implementation plan

Status: **PLAN ONLY — nothing implemented.** Companion to `00-plan.md`
(the V1/V2 vol desk that needs the hedge), `01-perps-venues.md` (venue
findings), and `02-ember-bluefin.md` (how Ember does it, and why we want
stronger guarantees). This documents the agreed custody design and the
build phases for using Bluefin Pro as the vault's perp hedge venue.

## 1. The problem this design solves

A curated trading vault must let the curator hedge on Bluefin Pro while
preserving the vault's core invariant: **the curator can never move
vault funds to themselves.** Bluefin makes Move-object custody
impossible — accounts are keyed by an address whose authority is a
detached ed25519 signature verified inside Bluefin's Move code, and
withdrawals can only pay the account's own address. Ember's answer was a
trusted operator + MPC wallets + posted NAV (a managed fund). Ours keeps
the invariant by construction, with a jointly-controlled parent account
and a policy-enforcing co-signing service.

## 2. The custody design (agreed)

Actors and keys:

- **Parent account** — the Bluefin Pro account. Its key is a **2-of-2
  threshold-ed25519 key (MPC/FROST)**: one share held by the protocol's
  signing service, one by the curator. Externally it is a plain ed25519
  pubkey + plain signatures, so Bluefin's verifier accepts it, and the
  derived Sui address is a normal address. Neither party can sign alone.
- **Curator wallet** — the curator's ordinary key, registered as the
  parent's **authorized wallet** on Bluefin: can place/cancel orders,
  adjust margin/leverage, manage positions; explicitly cannot deposit
  or withdraw. Day-to-day trading needs no ceremony.
- **Signing service** ("hedge-signer", gas-station-shaped): holds the
  protocol share, runs the co-signing ceremony, and enforces policy on
  everything it signs. Append-only audit log; every signature is an
  auditable event.

The custody loop (every exit requires the protocol co-signature):

```
vault ──(attested + budgeted release)──▶ parent Sui address
      ──deposit_to_asset_bank(credit parent)──▶ Bluefin AssetBank
      ──curator trades via authorized wallet (own key, no ceremony)──
      ──withdraw (parent-signed payload; ONLY pays parent address)──▶ parent Sui address
      ──(2-of-2 Sui tx, policy: outputs → vault only)──▶ vault sweep (receive path)
```

Two structural gifts from Bluefin's own design:

1. Withdrawals have **no destination field** — funds can only land back
   at the parent address. Withdraw payloads are therefore safe to
   co-sign unconditionally; the real policy gate is the Sui-side hop.
2. Accounts materialize on first deposit (`deposit_to_asset_bank`
   credits any address, no signature from the credited account), so no
   parent signature is needed to create the account.

### Why threshold-ed25519, not Sui-native multisig (evidence)

Sui multisig lives in the transaction-sender authentication layer. The
parent never *sends* Bluefin transactions — the sequencer does, carrying
the parent's authority as signature bytes verified in Move. Confirmed
against the mainnet package (`0xe744…85b7`): both
`exchange::authorize_account` and `exchange::withdraw_from_bank` take
`vector<u8>` payload + `vector<u8>` signature parameters. Move has no
Sui-multisig verifier; the SDK and on-chain surface are single-key
ed25519. A native multisig parent would fail at the one-time authorize
call (curator never becomes tradable) and at every withdrawal (deposits
become one-way). Threshold-ed25519 emits plain ed25519 signatures and
sidesteps the whole question.

### What is enforced where

| Guarantee | Enforcement |
|---|---|
| Vault funds can only be released to the registered hedge address | **Move** (attested address on the vault) |
| Maximum external exposure (% of NAV, rate-limited) | **Move** (release budget) |
| Bluefin withdrawals only pay the parent address | **Bluefin protocol** (no destination field) |
| Parent-address funds only move back to the vault | **Signing service policy** (template check on the 2-of-2 Sui tx) + curator share (protocol can't move them alone either) |
| Only the registered curator wallet gets authorized | **Signing service policy** (gates `authorize_account` payloads) |
| Curator trades within venue limits | **Bluefin** (authorized-wallet permission set) |
| Hedge equity enters NAV honestly | **Move** (attestation guardrails: max delta/update, min interval) + keeper attestation |
| Adversarial trading (self-crossing value out through the book) | **NOT preventable by signatures** — bounded by the Move release budget, detected by reconciliation monitoring |

That last row is the honest limit of the design: trading is slow
withdrawal. The release budget is what caps the blast radius, which is
why "any transfers to the attested address" is deliberately NOT the
design — releases are budgeted.

## 3. Components to build

### 3a. Contracts (trading-vault + a small adapter)

- **Hedge address registry**: per-vault `hedge_address` (the parent's
  Sui address), set by admin (or curator-proposed, admin-approved), with
  a rotation story.
- **Budgeted release**: `release_to_hedge<T>(vault, cap, amount)` —
  curator-gated, transfers only to the registered address, enforcing a
  cap (% of NAV at release time) and a rate limit (X%/day). Emits
  events; tracked as the vault's `external_exposure`.
- **External-position appraisal**: the released capital becomes an
  appraisable "external position" — a new oracle-adapter witness
  (`hedge_oracle`) mints a `PriceAttestation`-style equity attestation
  posted by the keeper from Bluefin account equity, consumed with
  Ember-style guardrails enforced on-chain: max delta per update, min
  update interval, staleness backstop. Return flow (sweeps arriving at
  the vault address) reduces `external_exposure`.
- Follows the SO-297 pattern: no vault-core changes; an oracle adapter
  plus one registry entry.

### 3b. Signing service (`hedge-signer`)

- FROST 2-of-2 threshold-ed25519: keygen ceremony (per vault),
  two-round signing with the curator's client. Library selection in
  Phase 0 (mature Rust FROST implementations exist).
- **Policy engine**, evaluated before contributing a signature share:
  - *Bluefin login payload* (parent JWT for the authorize call): allow.
  - *`authorize_account` payload*: allow only for the vault's registered
    curator wallet; deny all others.
  - *Withdraw payload*: allow (destination is forced by protocol);
    log amount/asset.
  - *Sui transaction from the parent address*: parse `TransactionData`,
    allow only when every output pays the vault (sweep path) or gas
    under a limit — the gas-station template-check muscle applied to
    outbound policy.
  - Everything else: deny.
- Append-only signed audit log; `alert_id` events on every denial and
  every co-signed withdrawal/sweep.
- Key-loss posture (Phase 0 decision): 2-of-3 with a cold recovery
  share vs documented 2-of-2 share-backup ceremonies. Note Bluefin
  accounts cannot rotate keys — recovery from key loss means
  withdraw-and-migrate (which itself needs both shares), so backups are
  load-bearing.

### 3c. mm-bot `HedgeVenue` implementation

- `bluefin` impl of the `HedgeVenue` trait from `00-plan.md`, using the
  official `bluefin-pro` Rust SDK with the **curator/bot authorized
  wallet** (own key — no signing ceremony on the trading path):
  positions, margin headroom, order placement, live + historical
  funding into the pricing engine's hedge-cost term.
- Band-based rebalancing logic stays venue-agnostic above the trait.
- Sweep/repatriation requests go through the signing service.

### 3d. Keeper

- **Equity attestation cron**: read parent-account equity (REST
  `GET /account` cross-checked against on-chain IDS state), post the
  hedge attestation within guardrails.
- **Reconciliation monitor**: vault `external_exposure` vs releases −
  sweeps vs Bluefin equity; divergence beyond tolerance ⇒
  `alert_id = "hedge-reconciliation"`. This is the adversarial-trading
  detector.
- Sweep crank: funds landing at the vault address enter via the
  existing receive path.

### 3e. Curator dashboard (hedge panel)

- Vault spot trading needs no new venue (existing deepbook-adapter
  curator functions + spot-pool allowlisting) — separate work item.
- Hedge panel: release-to-hedge (budget-aware), Bluefin positions/margin
  view (their TS SDK, authorized-wallet login), withdraw + sweep-back
  flow driving the signing-service ceremony, funding + equity history.

## 4. Phases

**Phase 0 — de-risk (no code beyond spikes)**
1. Ask Bluefin: any support (now/planned) for Sui-multisig signature
   verification, delegated-withdrawal destinations, or account key
   rotation. Outcome only changes the substrate, not the design.
2. Staging validation on their Sui-testnet env (`sui-staging`):
   create account by deposit → authorize a second wallet → trade via
   authorized wallet → confirm it cannot withdraw → parent-signed
   withdraw lands at parent address. First with plain keypairs, then
   with a FROST-signed parent.
3. Pick the FROST library; decide 2-of-2 + backups vs 2-of-3.
4. Confirm `bluefin-pro` crate covers the payload shapes we must
   co-sign (or we sign raw payloads ourselves).

**Phase 1 — contracts**: hedge address registry + budgeted release +
external-exposure tracking + hedge-equity oracle adapter, with Move
tests; redeploy.

**Phase 2 — signing service**: keygen + ceremonies + policy engine +
audit log; end-to-end on Bluefin staging with a test vault: release →
deposit → authorize → trade → withdraw → sweep, every policy branch
exercised (wrong recipient denied, foreign authorize denied).

**Phase 3 — keeper + mm-bot**: equity attestations, reconciliation
alerts, `HedgeVenue::bluefin` with funding feed; paper→staging soak.

**Phase 4 — dashboard hedge panel.**

**Phase 5 — mainnet readiness** (gated, out of scope until scheduled):
key ceremonies with real custody posture, budget parameters, runbooks
(sequencer outage, service outage, share loss), monitoring/alerts.

## 5. Risks

- **Adversarial trading** (curator self-crosses value to their own
  account): not signature-preventable; bounded by the release budget,
  surfaced by reconciliation. Accept + monitor.
- **Bluefin liveness**: withdrawals are sequencer-executed; a halted
  sequencer freezes exit (no force-exit exists). Exposure bounded by
  the same budget; document as venue risk.
- **Key loss**: 2-of-2 share loss strands the account (no rotation);
  mitigated by backups or 2-of-3. Phase 0 decision.
- **Service downtime**: trading unaffected (curator's own key);
  repatriation and new authorizations pause. Acceptable degradation.
- **Payload drift**: Bluefin payload formats may change under us
  (pre-release SDK cadence); pin SDK versions, staging canary.
- **NAV gaming via attestation**: bounded by on-chain guardrails
  (max delta, min interval) exactly as designed for operator-attested
  inputs in `02-ember-bluefin.md`.
