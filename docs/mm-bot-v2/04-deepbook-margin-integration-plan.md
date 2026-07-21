# DeepBook Margin hedge integration — implementation plan

Status: **PLAN ONLY — nothing implemented.** Companion to
`03-bluefin-integration-plan.md`: this fits DeepBook Margin (DBM) into
the same custody framework — jointly-controlled parent address, policy
co-signing service, attested + budgeted vault releases — and calls out
where DBM is structurally different (mostly: simpler crypto, stronger
policy and NAV, weaker economics and liveness).

Verified source facts this plan builds on (see `01-perps-venues.md` and
the margin_manager.move re-verification): `MarginManager` is `has key`
only, force-shared via a hot-potato initializer, cannot be wrapped or
custodied; every user-facing operation asserts
`ctx.sender() == owner`; `owner` is fixed at creation to the creating
tx's sender, with no setter and no cap. Short = `borrow_base` → sell on
the spot book via `pool_proxy`. Liquidation is permissionless at
oracle-priced risk ratios (SUI/USDC: liquidate at 1.1, borrow gate
1.25, 5x max).

## 1. How the framework maps onto DBM

The Bluefin design translates almost mechanically, with three upgrades
and two downgrades:

| Framework piece | Bluefin | DeepBook Margin |
|---|---|---|
| Parent identity | 2-of-2 **threshold-ed25519 (FROST)** — required because Bluefin verifies detached sigs in Move | **Native Sui 2-of-2 multisig address** — everything is ordinary Sui tx sending, where multisig natively works. No MPC cryptography at all. |
| Account object | Bluefin internal ledger entry (keypair-backed) | `MarginManager` shared object, `owner` = the multisig address (created by a multisig-sent tx) |
| Curator trading | Free-flowing via authorized wallet (own key) | **No delegation exists** — every action is owner-sent, i.e. every trade goes through the co-signing ceremony |
| Policy visibility | Withdrawals + authorize only; trading invisible to the service | **Every operation** passes the service: it can enforce max leverage, borrow caps, band compliance, order-size limits — trade-level policy Bluefin can't give us |
| Exit gate | Bluefin withdraw pays parent address only; Sui-leg policy | `withdraw` returns a Coin inside the PTB — the service must validate the **whole transaction**: all outputs end at the vault (or stay inside the manager) |
| Hedge NAV | Keeper-attested equity (trusted, guardrailed) | **Trustlessly computed on-chain**: the manager is a readable shared object; an oracle adapter can appraise assets − debt with Pyth legs inside the appraisal PTB itself |
| Hedge carry | Funding (shorts often EARN) | Borrow APR (**always a cost**, kinked curve) |
| Atomic exercise+hedge | Impossible (off-chain matching) | **One PTB**: flash-borrow strike → exercise → sell underlying → repay flash loan → `place_market_order_and_repay_loan` — sender is the multisig (vault cranks are permissionless, so the multisig can drive the whole combined tx) |

Downgrades to respect: the carry is always negative, leverage caps out
at 5x (SUI/USDC), and — the important one — **service liveness becomes a
margin-safety dependency** (see §4).

## 2. The custody loop

```
vault ──(attested + budgeted release)──▶ multisig Sui address
      ──2-of-2 tx: margin_manager::new / deposit ──▶ MarginManager (owner = multisig)
      ──2-of-2 txs: borrow_base → pool_proxy sell (short on), repay/adjust (rebalance)──
      ──2-of-2 tx: withdraw → outputs policy-checked to vault address──▶ vault sweep
```

Every arrow after the release is a Sui transaction **sent by the
multisig**, so every arrow requires the protocol co-signature and passes
the policy engine. The curator alone can do nothing; the protocol alone
can do nothing. Unlike Bluefin there is no "free trading lane" — that
is both the cost (latency, availability) and the benefit (total policy
coverage).

## 3. Components

### 3a. Contracts — shared with the Bluefin plan, plus one upgrade

- **Hedge address registry + budgeted release + external-exposure
  tracking**: identical primitives to `03-…` §3a — build once, serve
  both venues. The registered hedge address here is the multisig.
- **On-chain hedge appraisal (the upgrade)**: a `dbm_oracle` adapter
  witness whose attestation is *computed, not attested*: read the
  `MarginManager` (public getters / `calculate_assets` /
  `calculate_debts`), price base/quote legs with the same Pyth
  attestations the appraisal already mints, emit the equity as a
  `PriceAttestation`-style leg. No keeper trust, no guardrail
  parameters — the Ember-style max-delta/min-interval machinery is
  unnecessary for this venue. (Keep it only for the Bluefin leg.)

### 3b. Signing service — same service, a richer policy module

Same `hedge-signer` from the Bluefin plan; DBM adds a policy module
that parses full `TransactionData` and classifies commands:

- **Auto-approve tier** (value stays inside the manager): `deposit`
  (from the multisig's own balance), `borrow_base/quote`, `repay_*`,
  `pool_proxy` order placement/cancel/modify, TPSL management,
  `update_current_price`, registry ops — subject to risk policy:
  max effective leverage (target ≤2x per `01-…` stress note), max
  borrow amount, allowed pools only, order-size ceilings.
- **Strict tier**: any tx where a `Coin` output leaves the
  multisig/manager perimeter — approve only when every terminal output
  pays the vault address (sweep) or gas under a limit. This is the
  gas-station template check generalized to arbitrary PTBs; deny by
  default on unrecognized commands.
- **Emergency tier**: margin top-ups (deposit to restore risk ratio)
  pre-approved and fast-tracked (see §4).
- No Bluefin-payload handling needed here — no detached-signature
  surface exists; it's all Sui txs. Native multisig aggregation replaces
  the FROST ceremony (curator signs, service verifies + countersigns,
  either party can assemble and submit).

### 3c. mm-bot `HedgeVenue::deepbook_margin`

- Implements the trait with: position = manager state read
  (assets/debt/risk ratio via getters + indexer), hedge-cost input =
  live borrow APR (`margin_pool::interest_rate()`) instead of funding,
  adjust = build the borrow/sell or buy/repay PTB and submit it through
  the signing ceremony.
- Band rebalancing tolerates the co-sign round-trip (our own service;
  bands are %-of-NAV triggers, not latency-sensitive).
- Slippage model: hedge trades hit the DeepBook spot book — feed book
  depth into the bid's hedge-cost term (the spec's slippage estimate).

### 3d. Keeper

- **Risk-ratio monitor**: watch manager risk ratio (indexer has
  dedicated liquidation-monitoring endpoints) with two thresholds:
  warn (`alert_id = "dbm-margin-warn"`) and act — trigger the
  emergency top-up flow or a reduce-only unwind well above the 1.1
  liquidation line.
- **Reconciliation**: vault `external_exposure` vs releases − sweeps vs
  on-chain manager equity. (Cheaper than Bluefin's — all on-chain.)
- Flash-exercise + hedge-unwind composition (from `00-plan.md` §5) —
  on this venue it's a single PTB the keeper can pre-build and route
  through the ceremony.

### 3e. Dashboard

Same hedge panel as the Bluefin plan, minus the venue login: positions,
risk ratio, borrow rate, release/sweep flows — all reads are Sui RPC +
indexer. Curator-side signing UX: the panel builds the PTB, curator
signs with their multisig member key, service countersigns, submit.

## 4. The liveness problem (DBM-specific)

Margin top-ups require an owner-sent `deposit` — a 2-of-2 tx. If the
signing service (or the curator) is unavailable during a violent rally,
the short can reach the 1.1 liquidation ratio and get liquidated
(~5% penalty) before anyone can react. Bluefin does not have this
problem shape (the bot adjusts margin unilaterally as an authorized
wallet). Mitigations, in order of preference:

1. **Run under-levered**: effective leverage ≤2x makes the liquidation
   move ≈ +50–80%, aligned with the V1 stress buffer sizing.
2. **Buffer inside the manager**: keep the hedge-margin stress multiple
   as quote balance *inside* the MarginManager (it counts toward the
   risk ratio without any tx needed).
3. **Emergency fast-track**: pre-agreed top-up policy in the service
   (auto-approve deposits that raise the risk ratio), redundant service
   deployment, and both-party alerting on the warn threshold.
4. Accept the residual: a liquidation is a bounded fee (~5% of the
   liquidated notional), not a custody loss — funds return to the
   manager perimeter.

## 5. Phases

**Phase 0 — de-risk (no code beyond spikes)**
1. Testnet reality check: which testnet pairs are margin-enabled
   (SUI/DBUSDC, DEEP/DBUSDC, DBTC/DBUSDC presumed — read the on-chain
   registry), their leverage/risk params, and whether the canonical
   testnet books have enough depth to exercise the mechanics at all.
2. Multisig UX spike: create a 2-of-2, send a MarginManager creation tx
   from it, confirm `owner` lands as the multisig address, and measure
   the sign-aggregate-submit loop we'd wrap in the service. Confirm gas
   funding posture for the multisig address (or sponsored-tx via our
   gas station).
3. Decide 2-of-2 vs 2-of-3 (same decision as Bluefin Phase 0 — one
   answer for both venues; native multisig makes 2-of-3 trivial here).
4. Verify `calculate_assets`/`calculate_debts`/getter surface is
   callable from an external package for the on-chain appraisal leg
   (agent findings say yes — confirm against the deployed testnet
   package).

**Phase 1 — contracts**: the shared hedge primitives (registry, budget,
exposure) + the `dbm_oracle` on-chain appraisal adapter; Move tests;
redeploy.

**Phase 2 — signing service**: native-multisig ceremony + the
three-tier policy module; end-to-end on testnet: release → create
manager → deposit → borrow/sell (short on) → rebalance → repay →
withdraw-to-vault → sweep, plus denial tests (withdraw to foreign
address, over-leverage borrow, unknown command).

**Phase 3 — keeper + mm-bot**: risk monitor, reconciliation,
`HedgeVenue::deepbook_margin` with borrow-APR feed; staging soak
against the sim.

**Phase 4 — dashboard hedge panel.**

**Phase 5 — mainnet readiness** (gated): real key ceremony, budget +
leverage parameters, runbooks (service outage during rally, liquidation
event, share loss), monitoring.

## 6. Build-order recommendation vs Bluefin

Do the **shared primitives first** (vault registry/budget/exposure +
signing-service skeleton — Phases 1–2 here largely double as Bluefin
Phases 1–2), then DBM's venue module before Bluefin's: no external
dependency (no MPC library, no venue emails, no detached-payload
handling), testnet works today, and it exercises the full policy engine
harder. Bluefin's venue module (FROST substrate + payload policy +
funding feed) layers on afterwards for the funding economics at scale —
per `01-perps-venues.md`, run both behind `HedgeVenue` and let realized
hedge cost pick the mix.

## 7. Risks

- **Liveness → liquidation** (§4): the venue-defining risk; mitigated
  by low leverage + in-manager buffer + fast-track top-ups; residual is
  a bounded ~5% fee, not fund loss.
- **Always-negative carry**: borrow APR spikes with pool utilization
  (docs show 12%→62% between 80–90% util on USDC); the bid's hedge-cost
  term must read live rates, and the desk must be willing to shrink the
  book when carry is prohibitive.
- **Thin margin universe**: 5x max on SUI/USDC only; WAL/DEEP at 3x; no
  BTC pair confirmed on mainnet. Fine for a SUI-underlying book;
  a constraint for anything else.
- **Adversarial trading**: still not signature-preventable (the curator
  co-signs "legitimate" trades that leak edge) — but on this venue the
  service sees every order and can enforce price-band sanity checks
  against the oracle, materially shrinking the channel vs Bluefin.
  Release budget remains the hard bound.
- **Oracle dependency**: DBM liquidation runs on Pyth with its own
  staleness/confidence config; our appraisal uses our own Pyth legs —
  two configs to keep coherent.
- **Upgrade churn**: the margin package is young and upgrading
  (version-gated registry); pin against testnet, watch releases.

## Phase 0 findings (2026-07-21, SO-301 follow-up)

Testnet reality check (spike 1) — read live via publicnode RPC:

- Canonical testnet ids (from `@mysten/deepbook-v3` 1.5.8 constants,
  verified on-chain): margin package (latest)
  `0xd6a42f4df4db73d68cbeb52be66698d2fe6a9464f45ad113ca52b0c6ebd918b6`,
  ORIGINAL package (type tags)
  `0xb8620c24c9ea1a4a41e79613d2b3d1d93648d1bb6f6b789a7c8f261c94110e4b`,
  `MarginRegistry`
  `0x48d7640dfae2c6e9ceeada197a7a1643984b5a24c55a0c6c023dac77e0339f75`
  (versioned, v1).
- Four margin-enabled pools; the one we care about, SUI/DBUSDC
  (`0x1c19362ca52b8ffd7a33cee805a67d40f31e6ba303753fd3a4cfdfacea7163a5`),
  is `enabled: true` with mainnet-identical risk params: liquidate 1.1,
  borrow gate 1.2499, withdraw 2.0, target 1.25; liquidation rewards 2%
  user + 3% pool. Margin pools: SUI
  `0xcdbbe6a72e639b647296788e2e4b1cac5cea4246028ba388ba1332ff9a382eea`,
  DBUSDC
  `0xf08568da93834e1ee04f09902ac7b1e78d3fdf113ab4d2106c7265e95318b14d`.
- 71 margin managers already registered on testnet — the mechanics are
  exercised there.
- Spikes 2–3 (multisig round-trip timing, gas posture) fold into the
  staging hedge-signer bring-up, which performs exactly that ceremony.
- Item 4 (getter surface callable externally) was settled earlier by the
  dbm-oracle e2e test.
- Decision (2026-07-21): custody posture is 2-of-2 with documented share
  backups for V1 staging; revisit 2-of-3 with a cold share before
  mainnet. Native multisig on DBM makes either trivial; Bluefin's FROST
  substrate supports both.
