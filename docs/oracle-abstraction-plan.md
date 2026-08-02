# Oracle abstraction — Pyth upgrade, Switchboard integration, and a one-field switch

**Goal:** be able to switch the entire stack between oracle providers by
executing **one on-chain transaction** and changing **one backend config
field** — while keeping multiple providers allowlisted in parallel, and
keeping the Pyth path alive so we can return to it when budget allows.

**End state of this work:** Switchboard is the live provider; Pyth is
upgraded, published and dormant — revivable with one tx and one field.

Companion: [`pyth-transition.md`](pyth-transition.md) (the usage inventory).

---

## 0. The two facts that shape everything

### 0.1 The on-chain half is already done

`contracts/trading-vault/sources/price.move` is the abstraction we need and
it already exists:

```move
public struct PriceAttestation has copy, drop { oracle, asset, quote_asset, price, timestamp_ms }

public fun attest<W: drop>(_witness: W, reg: &OracleRegistry, …): PriceAttestation {
    let oracle = type_name::with_defining_ids<W>();
    assert!(registry::is_oracle_allowed(reg, &oracle), errors::oracle_not_allowed());
    …
}
```

Three properties matter:

1. **Adding a provider is a new package + one `allow_oracle` call.** Nothing
   in `trading_vault` changes. `contracts/trading-vault/Move.toml` has no
   oracle dependency at all.
2. **Parallel operation is free.** `allowed` is a `VecSet<TypeName>`. Allowlist
   both witnesses and both work simultaneously.
3. **The kill switch is instant and total.** `PriceAttestation` is
   `copy, drop` and is *never stored* — there is no stale on-chain price to
   purge. `disallow_oracle(PythOracle)` takes effect on the very next
   transaction.

So the on-chain answer to "turn an oracle on and off at will" is already
`allow_oracle` / `disallow_oracle`, AdminCap-gated, one tx.

### 0.2 The off-chain half is Pyth all the way down

Six places hardcode Pyth. This is the actual work:

| Leak | Where | Why it blocks a switch |
|---|---|---|
| Internal gateway API is keyed by **Pyth's identifier type** | `oracle_client::price(feed: PriceFeedId)`, `protocol-types/src/pyth_id.rs` | Every consumer speaks Pyth ids |
| Catalog stores **one provider's** feed key | `TokenSpec.pyth_feed_id`, token-info DB column | No place to put a Switchboard feed hash |
| PTB composer calls the adapter **by name** | `sui-tx/tx/appraisal.rs:639` → `oracle_pyth::attest`; `PriceLegs { pyth: &PythHandles }` | Provider is compiled in |
| The update **prefix shape** is Pyth-specific | `sui-tx/tx/pyth_update.rs` (wormhole VAA → 4 calls) | Switchboard's shape differs |
| The browser builds the same prefix itself | `frontend/src/tx/appraisal.ts` (1055 lines) | Frontend would need a redeploy to switch |
| Gas-station allowlist pins Pyth call shapes | `sui-tx/tx/template.rs::PythPkgs` | Unsponsored deposits after a switch |

**Everything below is about closing those six.**

---

## 1. Deadline, decisions, and what is left to pin

Both open spikes are now settled — B decided (QuoteVerifier), C closed by
SO-334. The only unknowns left are implementation pins, called out inline.

### ⚠️ Pyth's Sui migration deadline is **2026-08-18** — just over two weeks out

Per [Pyth's Sui upgrade guide](https://docs.pyth.network/price-feeds/core/upgrade/preparing/sui):

- An API key is required for **everyone** who calls Hermes.
- `Move.toml` rev changes `sui-contract-{mainnet,testnet}` →
  `sui-pro-compatible-contract-{mainnet,testnet}`.
- **"There is no automatic upgrade path on Sui. Apps reference the Pyth
  package by object ID"** — the new revision is a *different on-chain
  package*, so `oracle-pyth` must be **republished**.

`rust-backend/crates/runtime-config/src/secrets.rs` already says a key is
"mandatory for Pyth Core access from 2026-07-31" — a date that has passed, so
assume the current integration is already degraded or dead.

### Decision: don't diagnose, just upgrade

**We are not spiking whether Pyth is currently broken.** No users, no official
deployment — if it is already gated, the answer is the same either way: do the
upgrade so Pyth is *ready* when we want it back, and cut over to Switchboard
in the meantime.

This has one consequence worth planning around rather than discovering:
**Phase 1 is a refactor whose exit criterion is "behaviour-identical",** which
needs a working provider to compare against. If Pyth's data plane is already
dead, there is no baseline. That is why Phase 2a (below) is split out and runs
first — it is cheap, needs no redeploy, and restores the baseline whether or
not it was broken.

### Spike B — DECIDED: **Quotes / QuoteVerifier**

Two integration shapes exist in Switchboard's material:

| Model | On-chain shape | Update flow |
|---|---|---|
| **Aggregator** | Shared `Aggregator` object per feed; `aggregator.current_result()` | `fetchUpdateTx` refreshes the shared object — *structurally like Pyth* |
| **Quotes / QuoteVerifier** | No per-feed shared object; `Quotes` verified in-PTB against a `QuoteVerifier`, keyed by `feed_hash` | `Quote.fetchUpdateQuote(...)` returns a value passed into your call |

**Use QuoteVerifier.** The Aggregator page
(`product-documentation/data-feeds/sui`) now **404s**; the live
`docs-by-chain/sui` tree has five pages and every one of them is
QuoteVerifier or Surge. It is both the better-documented and the current
standard.

It is also the better fit. No per-feed shared object means no refresh prefix,
no update fee, and no shared-object contention — and a `Quotes` value verified
in-PTB maps almost 1:1 onto our in-PTB `PriceAttestation`. The adapter reduces
to *verify quotes → assert feed hash → mint attestation*, materially simpler
than the Pyth adapter's four-call accumulator prefix.

**Still to pin at implementation time:** published package ids disagree
between Switchboard's own sources — the tutorial and the GitHub README give
different mainnet *and* testnet ids. Take them from one authoritative source;
do not copy from this document.

Spike B must also confirm: feed hashes exist for TBTC/TSUI/TUSDC/TWAL (+DEEP),
and that our already-deployed Crossbar can resolve them. Note the SO-333
commit message: Switchboard's non-mainnet queue lives on **Solana devnet**,
and Crossbar currently points at the public `api.devnet.solana.com`.

### Spike C — CLOSED: no non-swappable Pyth dependency remains

This spike asked whether DeepBook Margin forced Pyth to stay live. It did:
`deepbook_margin` took Pyth `PriceInfoObject`s by reference on every margin
write, and it was Mysten's package on Mysten's deployment — not ours to
republish against a different oracle.

**SO-334 removed the DeepBook Margin hedge integration entirely**
(`contracts/dbm-oracle`, `mm-bot/desk/dbm.rs`, the keeper `[external.dbm]`
legs, the `DbmLegInfo` plumbing, hedge-signer's margin perimeter, and the
frontend DBM discovery). Bluefin Pro is the sole planned hedge venue, and
`equity-oracle` is the only equity path.

Re-verified against `HEAD` after that commit: **every remaining Pyth consumer
in the tree is ours.** No third-party package receives a `PriceInfoObject`
from any PTB we build. The only live Pyth surfaces are
`oracle_pyth::attest` and the `pyth_update.rs` prefix that refreshes the
objects `attest` reads — both of which this plan replaces.

**Consequence: the cutover can be total.** Earlier drafts of this plan scoped
it to "vault appraisal only" and kept Hermes permanently on life support for
the hedge. That constraint is gone:

- `disallow_oracle(PythOracle)` removes Pyth from the on-chain surface
  completely, not partially.
- `pyth_update.rs` and the Hermes accumulator path go **dormant** rather than
  staying load-bearing.
- Under Switchboard's QuoteVerifier model there is no shared price object to
  refresh at all, so the entire update-prefix concept disappears from the hot
  path once Pyth is retired.

Phase 2a is therefore no longer a hedge-safety requirement. It is still worth
doing for the two reasons in §1: it gives Phase 1 a baseline to verify
against, and it leaves Pyth genuinely revivable.

> Note: `contracts/dbm-oracle/` still exists on disk as untracked `build/`
> leftovers from before the removal. Nothing tracked remains under it.

---

## 2. Target architecture — where the single switch lives

The switch cannot live in one place *only*, because oracle data crosses two
planes with different consumers:

```
                       ┌──────────────────────────────────────┐
   [ Pyth Hermes ]────▶│                                      │
                       │           oracle-service             │
   [ Crossbar   ]────▶ │   [oracle] provider = "…"   ◀── THE FIELD
                       │                                      │
                       ├──────────────────┬───────────────────┤
                       │   DATA PLANE     │  TRANSACTION PLANE│
                       │  /prices/:coin   │  /oracle/descriptor
                       │  /vol/realized   │  /oracle/legs     │
                       │  WS /ws          │                   │
                       └────────┬─────────┴─────────┬─────────┘
                                │                   │
              mm-bot, scheduler,│                   │ sui-tx::tx::oracle
              market-sim, keeper│                   │ frontend appraisal.ts
                                ▼                   ▼
                        (zero changes on     (build PTB legs from
                         a provider flip)     returned descriptor)
                                                    │
                                                    ▼
                                    on-chain: OracleRegistry allowlist
                                    allow_oracle / disallow_oracle
```

**Data plane** — spot and realized vol. Already centralized in
`oracle-service`; the only problem is that its API is keyed by
`PriceFeedId`. Re-key by **coin type** and consumers become
provider-agnostic for free.

**Transaction plane** — the in-PTB price legs. This *cannot* be hidden inside
oracle-service, because the legs are part of a transaction that the keeper,
the gas station and the browser each build. Centralize it by having
oracle-service **serve a descriptor** rather than build the PTB:

```jsonc
// GET /oracle/descriptor
{
  "provider": "switchboard",
  "adapterPackageId": "0x…",          // oracle_switchboard
  "attestTarget": "oracle_switchboard::attest",
  "registryIds": { "feedRegistry": "0x…", "oracleRegistry": "0x…" },
  "prefix": { "kind": "switchboard-quotes", "packageIds": { … } },
  "feeds": { "0x…::tbtc::TBTC": "0x4cd1…" }
}

// GET /oracle/legs?assets=<coinType>,…&quote=<coinType>
{
  "provider": "switchboard",
  "prefixPayloads": ["base64…"],       // Hermes accumulator OR Crossbar quote bundle
  "perAsset": [ { "asset": "0x…::tbtc::TBTC", "feedKey": "0x4cd1…" } ]
}
```

Both the Rust composer (`sui-tx::tx::oracle`) and `appraisal.ts` become thin
interpreters of that descriptor. Neither knows a provider name.

**Why this gives a true one-field switch:**

| Component | What it takes to switch |
|---|---|
| mm-bot, option-scheduler, market-sim | nothing — data plane payload is identical |
| keeper, gas-station, frontend | nothing — they read the descriptor at runtime |
| gas-station template allowlist | nothing — **both** providers' shapes registered permanently (§3.5) |
| on-chain | `allow_oracle` once, ahead of time; `disallow_oracle` when retiring |
| **oracle-service** | **`[oracle] provider = "…"` + restart** |

The frontend already resolves `ORACLE_PYTH_PACKAGE_ID` from token-info at
runtime (`config.ts`), not from a `VITE_` build-time env — so this preserves
the no-redeploy property. Do not regress that.

---

## 2.5 Implementation status (SO-335)

| Phase | Item | State |
|---|---|---|
| 3 | `contracts/oracle-switchboard` adapter + tests | **done** — 11 tests, cross math pinned to oracle_pyth's vectors |
| 3.5 | Per-asset oracle pins in `OracleRegistry` | **done** — 8 tests |
| 2b | `oracle-pyth` → `sui-pro-compatible-contract-*` | **done** — API is source-compatible; all 6 adapter tests pass on the new rev |
| 1a | `switchboard_feed_id` in catalog + `OracleProvider` type | **done** — token-info migration 000002, `feed_for(provider)` |
| 1b | Data plane follows the provider | **done** — oracle-service discovers feeds via `feed_for(provider)`; `GET /prices/by-asset/:coin_type` keys spot by asset so consumers never touch a provider feed key |
| 1c | `sui-tx::tx::oracle` seam; `appraisal.rs` off `oracle_pyth` | **done** — `emit_price_legs` + `OracleLegs`, 8 tests |
| 1d | `/oracle/descriptor`; frontend reads it | **done** — `oracle-client::descriptor()`, `frontend/src/api/oracleDescriptor.ts` |
| 1e | gas-station registers both providers' PTB shapes | **done** — both deposit shapes sponsor from one template set |
| 3b | `crates/switchboard-client` over Crossbar | **done** — 6 tests |
| — | deployment-manager publishes + activates both adapters | **done** — both witnesses allowlisted, both feed registries seeded |
| 2a | `pyth-client` pro endpoint + `accessToken` | **partial** — `Authorization: Bearer` was already correct and the endpoint is config-driven; needs a live keyed-vs-unkeyed check |
| 4–5 | Soak, flip, `disallow_oracle` | ops, post-merge |

**The switch, end to end, is now implemented.** Flipping
`[oracle] provider` in oracle-service and restarting it moves the data
plane (feed discovery, `/prices/by-asset`), the descriptor, the Rust PTB
composer and the deployed browser bundle. Nothing else redeploys.

### What remains before it can actually be flipped

These are deployment/ops, not code:

1. **Seed `switchboardFeedId` in the catalog.** Every token needs a
   Switchboard feed hash, or `oracle-service` refuses to boot on
   `provider = "switchboard"` (deliberately — a half-seeded catalog would
   silently leave assets unpriceable).
2. **Redeploy the contracts.** `OracleRegistry` gained a field and
   `oracle-pyth` moved to a different on-chain Pyth package, so this is a
   redeploy, not an upgrade. It publishes and allowlists both adapters.
3. **Point Crossbar at a paid Solana devnet RPC** and set
   `[oracle] crossbar_url`.
4. **Set `[switchboard] package_id`** in gas-station config so
   Switchboard deposits sponsor.
5. **Verify the Crossbar response shape.** `switchboard-client` decodes
   defensively against a shape not pinned in public docs; the first live
   call is what confirms it.
6. **Resolve the toolchain finding below** before the redeploy.

---

## 3. Phases

Two ordering rules drive this:

1. **Centralizing comes before both integrations.** Phase 1 is a pure refactor
   with Pyth as the only provider — behaviour-identical, fully testable, and
   it is what makes phases 3–4 cheap instead of a second rewrite. Doing
   Switchboard first means building the abstraction twice.
2. **The Pyth upgrade splits in half, and the halves go in different places.**
   The off-chain half (endpoint + auth) needs no redeploy and restores the
   baseline Phase 1 is verified against, so it goes first. The on-chain half
   (rev bump → republish) is a redeploy, so it rides along with Switchboard's
   publish — one redeploy, both adapters.

```
  2a  Pyth off-chain upgrade      no redeploy    ← restores the baseline
   1  Centralize the seam         no redeploy
  2b  Pyth rev bump + republish  ┐
   3  Switchboard adapter        ┘ ONE redeploy, both adapters allowlisted
   4  Prove the switch, both directions
   5  Hard cutover to Switchboard; Pyth dormant
```

### Phase 2a — Pyth off-chain upgrade (first; no redeploy)

The data plane can be fixed and confirmed without touching a contract.

| # | Change |
|---|---|
| 2a-i | `pyth-client`: move to the upgraded Hermes endpoint and send the key as `accessToken` / `Authorization: Bearer`. Bump `@pythnetwork/pyth-sui-js` on the frontend side. |
| 2a-ii | Confirm keyed ≠ unkeyed this time — SO-252 found the old key gave *zero* rate-limit elevation, so do not assume the new one works because it is configured. |
| 2a-iii | Deploy `oracle-service` (+ keeper for its direct accumulator path). No contract change. |

**Verify:** `/prices` and `/vol/realized` serve real data for all five feeds.
That is the baseline Phase 1 measures against. If this cannot be made to work,
stop and re-plan — Phase 1 would be refactoring against a corpse.

### Phase 1 — Centralize the oracle seam (no provider change)

| # | Change | Verify |
|---|---|---|
| 1a | `TokenSpec.oracle_feeds: { pyth?, switchboard? }`; keep `pyth_feed_id` as a deprecated alias for one release. token-info migration + `deployments.json` shape. | catalog round-trips; old records still parse |
| 1b | Re-key `oracle-client` / `oracle-service` REST+WS by **coin type**. Delete `PriceFeedId` from consumer signatures (it stays internal to `pyth-client`). | mm-bot, scheduler, market-sim, keeper unchanged in behaviour |
| 1c | New `crates/sui-tx/src/tx/oracle/`: `trait OracleAdapter { descriptor(), prefix_legs(), attest_legs() }`; move `pyth_update.rs` under `oracle::pyth`. `appraisal.rs` stops naming `oracle_pyth`. | `trading-vault-smoke` green |
| 1d | `GET /oracle/descriptor` + `GET /oracle/legs` on oracle-service. Frontend `appraisal.ts` consumes them. | deposit e2e on staging, byte-identical PTB |
| 1e | gas-station: register both providers' template shapes (Switchboard's behind a feature-less `Option`, inert until 3). | template unit tests |

**Exit criterion:** grep for `pyth` outside `crates/pyth-client`,
`sui-tx::tx::oracle::pyth`, and `contracts/oracle-pyth` returns nothing
load-bearing.

### Phase 2b — Pyth on-chain upgrade (rides with Phase 3's redeploy)

Deadline-relevant: **2026-08-18**. Code-complete this before the date even if
the redeploy lands after, so the Pyth path is upgraded-and-ready rather than
stranded on a revision that no longer resolves.

| # | Change |
|---|---|
| 2b-i | `contracts/oracle-pyth/Move.toml` rev → `sui-pro-compatible-contract-{mainnet,testnet}`. With `dbm-oracle` gone (SO-334), `oracle-pyth` is the **only** package carrying a `pyth` dep — nothing forces the old rev to coexist, so the plain rev bump should suffice and the `rename-from = "pyth"` dual-dep form is unnecessary. |
| 2b-ii | Republish `oracle-pyth`. New package id into `deployments.json`. |

**Publish this together with Phase 3.** Redeploys here are expensive —
multisig, DBM ceremony, feed seeding, `mm-bot deploy-collateral`, market-sim
funding. One redeploy that lands *both* adapters is materially cheaper than
two, and it is what leaves Pyth allowlisted-and-current on the same day
Switchboard goes live.

**Verify:** appraisal + sponsored deposit e2e on Pyth *before* flipping to
Switchboard — this is the last point where Pyth is proven end-to-end, and
proving it is the whole reason for doing this phase at all.

### Phase 3 — Switchboard adapter

| # | Change |
|---|---|
| 3a | `contracts/oracle-switchboard`: `SwitchboardOracle` witness; `SwitchboardFeedRegistry` mirroring `PythFeedRegistry` (`TypeName → {feed_hash, decimals}`, `max_age_secs`, staleness/deviation guardrails **as registry state, not caller args** — same anti-loosening property); `attest<Asset,Quote>(…) -> PriceAttestation`. |
| 3b | `crates/switchboard-client` — Crossbar HTTP (self-hosted, already deployed at `/{env}/crossbar/`). Feed resolution + quote/bundle fetch. |
| 3c | `oracle::switchboard` implementing `OracleAdapter`. Per Spike B (QuoteVerifier): **no prefix** — the adapter contributes a verified-in-PTB `Quotes` value, so `prefix_legs()` returns empty and the descriptor's `prefix.kind` is `"none"`. Keep the trait's prefix hook anyway; Pyth still needs it. |
| 3d | Publish, seed the feed registry, `allow_oracle(SwitchboardOracle)`. **Do not disallow Pyth.** |
| 3e | Crossbar: move off public `api.devnet.solana.com` to a paid devnet RPC via the `options/<env>/crossbar` secret already wired by `render-secrets.sh`. |

### Phase 3.5 — Optional: per-asset oracle pinning

Today `attest<W>` lets **any** allowlisted adapter price **any** asset. With
two adapters allowlisted, a compromise of either prices the whole book. If
you want parallel operation to be *safe* rather than merely possible, add an
optional pin:

```move
// OracleRegistry
pins: Table<TypeName /* asset */, TypeName /* oracle */>,   // absent ⇒ any allowlisted
```

This also enables **gradual migration** — move TSUI to Switchboard, leave
TBTC on Pyth, compare, then move the rest. Cuttable scope, but it is the
single highest-value addition for "minimize on-chain oracle risk."

### Phase 4 — Prove the switch

1. Flip `[oracle] provider = "switchboard"`, restart oracle-service only.
2. Confirm **no** other service restarts, no redeploy, no frontend rebuild.
3. Run the full e2e: deposit, appraisal, withdrawal fulfil, mm-bot quote,
   scheduler roll.
4. **Flip back to Pyth. Then forward again.** Reversibility is the deliverable
   — a one-way switch is not what was asked for.
5. Add `alert_id = "oracle-attest-failed"` and a divergence monitor comparing
   both providers per asset (alert on > N bps).

### Phase 5 — Hard cutover to Switchboard; Pyth dormant

1. Run **both allowlisted** for N days with the divergence monitor live — this
   is also the cheapest possible correctness check on the new adapter, since
   Pyth is right there to disagree with it.
2. Flip the field to Switchboard; leave Pyth allowlisted as hot standby.
3. Once satisfied: `disallow_oracle(PythOracle)` — on-chain exposure drops to
   one adapter. Since SO-334 this is a **total** retirement, not a partial
   one: no third-party package needs Pyth any more, so Hermes traffic can go
   to zero, `pyth_update.rs` goes dormant, and under QuoteVerifier there is no
   shared price object left to refresh at all. The Pyth API key stops being an
   operational dependency and becomes something we re-acquire on the way back.
4. **Keep `contracts/oracle-pyth` published and its code alive**, and keep it
   in the Move CI matrix and the redeploy publish order so it cannot rot the
   way `options_vault` did. Returning to Pyth is then one `allow_oracle` tx
   plus one config flip.

Dormant must not mean unbuilt. The failure mode to avoid is discovering, six
months from now, that the Pyth adapter no longer compiles against the
then-current Sui framework — at which point "switch back" is a project again
rather than a transaction.

---

## 4. The switch runbook (what you actually execute)

```
  ON-CHAIN (once, ahead of time)
    allow_oracle(SwitchboardOracle)      # AdminCap, 1 tx

  SWITCH
    edit  services/oracle-service/config/config.<env>.toml
          [oracle] provider = "switchboard"
    deploy oracle-service                # 1 service

  RETIRE (only after soak)
    disallow_oracle(PythOracle)          # AdminCap, 1 tx
```

**Order matters — never invert it.** Appraisal cannot complete without at
least one allowlisted adapter that can price every held asset. Disallowing
before allowing wedges deposits *and* withdrawals. Always
`allow → verify → disallow`.

**Do not disallow the last remaining adapter** for any reason, including as a
panic response — that is not a pause, it is a freeze on user funds
(withdrawal fulfilment needs an appraisal too). The correct panic lever is
the existing per-vault pause, not the oracle allowlist.

---

## 5. Risks

| Risk | Mitigation |
|---|---|
| **Pyth's data plane is already dead, so Phase 1 has no baseline** | Phase 2a runs first and needs no redeploy. If it cannot be made to work, stop — do not refactor against a corpse; build Switchboard first and verify Phase 1 against it instead |
| Pyth ends up upgraded but never proven | Phase 2b's exit criterion is a full e2e **on Pyth**, executed before the flip to Switchboard. Skipping it means discovering the Pyth path is broken months later, when we want it back |
| Package ids disagree across Switchboard docs | Pin from one authoritative source at implementation time |
| Two redeploys instead of one | Fold Phase 2b + Phase 3 publishes together |
| Bluefin is not yet a live hedge venue (SO-334 leaves the desk paper-hedged) | Independent of this work, but it means a Switchboard price error has no hedging counterweight during the soak. Keep the divergence monitor (Phase 4.5) live for the whole parallel period |
| Switchboard testnet queue is on Solana **devnet** | Already known (SO-333); thread a paid devnet RPC through the existing crossbar secret |
| Frontend regresses to build-time ids | Descriptor must stay a runtime fetch; add a test asserting no `VITE_`-baked oracle ids |

---

## 6. What is explicitly *not* changing

- `trading_vault` core — no oracle dependency, untouched by all of this.
- `equity_oracle` — the **pinned** per-vault equity witness, and since SO-334
  the only equity path. It is a keeper-attested account-value source, not a
  market price feed; a price-provider switch does not touch it. Rotating it is
  `rotate_external_account`, a different operation.
- The hedge venue. SO-334 made Bluefin Pro the sole planned venue and removed
  DeepBook Margin; none of that is affected by the oracle choice, and this
  plan no longer depends on it either way.
- `contracts/vault` (`options_vault`) — deprecated and unpublished (SO-332);
  its Pyth coupling is not in scope and must not be revived by this work.
