# Hedge-venue exploration: DeepBook Margin vs Bluefin Pro

Status: exploratory findings, 2026-07-20. **Recommendation superseded by
SO-334 (2026-08-02): DeepBook Margin is removed; Bluefin Pro is the sole
planned hedge venue — see `06-dbm-removal.md`.** The venue findings below
stand as the record of why. Companion to `00-plan.md` (the V1/V2 vol-desk reset), which
needs programmatic SHORT exposure on the underlying to delta-hedge a
long-call book, live funding/borrow-cost inputs for bid pricing, a NAV
story for the hedge leg, and eventually a curator dashboard placing
spot + hedge trades for a curated trading vault.

## TL;DR

| | DeepBook Margin | Bluefin Pro |
|---|---|---|
| Instrument | Leveraged **spot** (borrow-and-sell) — no perps | True perpetual futures |
| Short mechanics | `borrow_base` → sell on the CLOB; one-sided debt per manager | Short perp, cross or isolated margin |
| Hedge cost | Borrow APR — **always a cost** (kinked curve, e.g. USDC 12%→62% between 80–90% util) | Hourly funding — shorts **earn** when funding is positive (V1's tailwind), pay when negative |
| Basis risk | **None** — it's a short of the actual asset | Perp basis + funding variance |
| Leverage | SUI/USDC 5x, WAL & DEEP 3x (mainnet today) | ~25x tiers on SUI/ETH, ~40x BTC |
| Composability | **Fully PTB-composable public Move functions**, no caller allowlist | Off-chain TEE matching; on-chain objects, but trading only via their API/sequencer |
| Vault custody of the position | ✗ `MarginManager` is shared, `ctx.sender() == owner`, no cap/transfer → must be owned by a signing key | ✗ Funds enter Bluefin's `AssetBank` credited to a keypair-backed account |
| Custody mitigations | Position is a **shared on-chain object** — a vault adapter can *read* assets/debt trustlessly for NAV | "Authorized wallet" delegation: bot can trade but **cannot withdraw**; parent key withdraws only to itself |
| Atomic exercise+hedge | ✓ one PTB: flash-borrow → exercise → `place_market_order_and_repay_loan` — the spec's "exercise sale and hedge unwind are the same trade", literally | ✗ impossible (matching is off-chain) |
| Testnet | ✓ deployed (SDK constants; SUI/DBUSDC, DEEP, DBTC test pools) + testnet indexer | ✓ full `sui-staging` env on Sui testnet, shared test keys, paper accounts |
| SDKs | Official TS SDK (`@mysten/deepbook-v3` margin contracts, PTB builders); Move source public | Rust (`bluefin-pro` 1.13), TS (`@bluefin-exchange/pro-sdk` 2.x), Python in-repo; OpenAPI |
| Fees | Standard DeepBookV3 pool fees + borrow interest; liquidation ≈ 5% (2% liquidator + 3% pool) | Maker 0.005% / taker 0.035% (SUI taker 0.1%) + per-trade gas fee; hourly funding |

**Recommendation (SUPERSEDED — see `06-dbm-removal.md`):** the original
call was to run BOTH behind the `HedgeVenue` trait, DeepBook Margin
first. SO-334 reversed it: DBM hedges one base asset per MarginManager
and has no BTC pair, so it could never cover this book; its carry is
always a cost where Bluefin's funding is a revenue line; and its
every-trade co-signing ceremony makes service liveness a margin-safety
dependency. Bluefin Pro is now the only planned venue. Neither venue lets
the vault custody the hedge at the Move level — that invariant becomes a
dedicated hedge-key + on-chain reconciliation story either way (details
below), which should be decided once, venue-independently.

## DeepBook Margin — findings

Sources: docs.sui.io/onchain-finance/deepbook/deepbook-margin (+ design,
margin-risks, contract-information, indexer subpages),
github.com/MystenLabs/deepbookv3 `packages/deepbook_margin` source,
blog.sui.io launch posts, `@mysten/deepbook-v3` SDK constants.

- **What it is**: margin layer over DeepBookV3 spot books (mainnet
  2026-01-22). Four shared objects: per-asset `MarginPool` (lending),
  per-user-per-market `MarginManager` (wraps a BalanceManager + its
  three caps), `MarginRegistry` (risk config). No perps.
- **Shorting**: `borrow_base` then sell via `pool_proxy`
  (`place_limit_order_v2`, `place_market_order_v2`, reduce-only
  variants, and `place_*_order_and_repay_loan`). Debt is one-sided per
  manager (base-debt = short, quote-debt = long). Protocol constants
  allow 20x; configured mainnet pairs: SUI/USDC 5x, WAL/USDC 3x,
  DEEP/USDC 3x. XBTC margin pool exists in SDK constants; its enabled
  pair is unconfirmed.
- **The custody catch**: `MarginManager` is force-shared with
  `owner: address` fixed to the creating tx sender; every mutator
  asserts `ctx.sender() == owner`. No ownership cap, no transfer
  function. A Move package **cannot wrap or custody one** the way we
  wrap a `BalanceManager`. The DeepBook "authorized apps" registry
  (`DeepbookAdminCap`-gated) only gates
  `new_with_custom_owner_caps_v2<App>` — creating BalanceManagers with
  arbitrary owners — and mainnet authorizes exactly one app,
  `MarginApp`. Getting our own witness authorized would require Mysten,
  and would still not make MarginManager custody possible.
- **What a vault CAN do on-chain**: read the shared manager
  (assets/debts/risk-ratio getters are public) — so an adapter can
  appraise the hedge account trustlessly inside our appraisal PTB even
  though it can't control it. And the **lending side is cap-based**:
  `SupplierCap has key, store` → the vault CAN custody lending
  positions (idle-reserve yield inside custody is a free side win).
- **Risk machinery**: Pyth-oracled (max age default 5 min, confidence
  bound, EWMA-deviation bound); permissionless liquidation (all orders
  cancelled, partial liquidation to target ratio); SUI/USDC thresholds:
  withdraw 2.0 / borrow 1.25 / liquidate 1.1 / target 1.25, rewards 2%
  liquidator + 3% pool; price-band assert on order placement (±5%
  default); order TTL clamp (3d).
- **Borrow economics**: kinked utilization curve; docs' USDC example
  12% APR at the 80% kink → 62% at 90% max utilization; live via
  `interest_rate()` or the indexer. For shorts we'd borrow the BASE
  (SUI) — SUI pool rates apply. Interest is a pure cost; there is no
  scenario where the short leg earns carry.
- **Integration surface**: all entries are ordinary `public fun`s, no
  caller allowlist; official TS SDK exposes them as PTB-builder
  callables; margin-specific indexer endpoints incl. liquidation-risk
  monitoring on mainnet + testnet. Testnet package/registry/pools are in
  the SDK constants (SUI, DBUSDC, DEEP, DBTC pools). Note: testnet
  margin rides Mysten's **canonical** testnet DeepBook, not our house
  deployment — fine (hedge venue ≠ options venue) but its books are
  near-empty, so testnet hedging is effectively paper-grade anyway.

## Bluefin Pro — findings

Sources: bluefin-exchange.readme.io/reference (how-it-works, auth,
orders, authorized-wallets, websockets, rate-limits, withdraw,
fundingRateHistory), live `GET /v1/exchange/info` on prod + staging,
mainnet/testnet RPC inspection of their Move packages,
github.com/fireflyprotocol/pro-sdk, crates.io/npm.

- **Architecture**: off-chain CLOB matching in a Nautilus TEE (<1ms),
  on-chain settlement on Sui; inputs/outputs/attestations published to
  Walrus. Funds live in a shared `AssetBank` (external data store);
  accounts/positions live in a sequence-hash-gated shared internal data
  store driven by Bluefin's operator address. Positions are table
  entries, not Sui objects.
- **Markets** (prod, live): BTC, ETH, SUI, SOL, DEEP, WAL, HYPE, GOLD
  perps; **USDC is the sole collateral**. SUI IMR 3.8%/MMR 1.8%; maker
  0.005%/taker 0.035% (SUI taker 0.1%) + small per-trade gas fee.
  Cross + isolated margin. Hourly funding, capped ±0.1%/h, public
  `fundingRateHistory` endpoint + on-chain funding events.
- **Account/auth**: accounts are Sui addresses; wallet-signature login
  (ed25519) → JWT; orders signed with the account key. **Authorized
  wallets**: a parent account can whitelist another wallet that can
  place/cancel orders, adjust margin/leverage, manage positions — but
  explicitly **cannot deposit or withdraw**. That maps cleanly to "the
  bot hedges but can never extract funds."
- **Custody**: anyone (including a vault PTB spending vault-custodied
  coins) can call `deposit_to_asset_bank` crediting any account address
  — but once inside, control is the account keypair's. Withdrawals are
  user-signed payloads executed by Bluefin's sequencer, with no
  destination field — funds appear to return only to the account's own
  address; no force-exit/escape hatch is visible in the package. A Move
  object can't be the account (it can't sign), so the parent key is
  necessarily a real keypair. Custody is organizational, with the
  useful property that the *trading* key (bot) can be the non-withdrawing
  authorized wallet.
- **Ops surface**: REST (create/cancel, leverage, isolated margin,
  withdraw, account, funding history) + WS market/account streams;
  rate limits 300 RPM general / 500 RPM trade gateway, MM partners can
  request more. Official **Rust SDK** (`bluefin-pro` crate) with
  signing utilities and Dev/Staging/Prod envs; TS SDK for the
  dashboard. Full staging deployment lives on **Sui testnet** (verified
  via RPC) with BTC/ETH/SUI/SOL/GOLD markets and shared test keys.
- **Product lineage**: Pro replaced the old v2/"Beta" exchange
  (migration completed July 2025); learn.bluefin.io pages describing
  MarginBank/wUSDCeth are stale v2 docs. AlphaLend is a separate
  product.

## The custody question (venue-independent decision)

Neither venue allows Move-enforced vault custody of the hedge leg:

- DeepBook Margin binds the position to a **signing address**
  (sender-auth, no cap).
- Bluefin binds funds to a **keypair-backed account** in their bank.

So the vault invariant ("curator cannot withdraw to self") cannot cover
the hedge margin cryptographically today. Practical containment, same
shape for both venues:

1. **Dedicated hedge key** distinct from the bot's quoting key; on
   Bluefin the bot trades via an authorized wallet that *cannot*
   withdraw; on DeepBook Margin the hedge key owns the MarginManager
   and its key custody is an ops concern.
2. **On-chain reconciliation**: DBM's manager is a readable shared
   object → the vault appraisal can value the hedge account
   trustlessly; Bluefin equity is readable via their getters/API and
   attestable, but with weaker on-chain guarantees.
3. **Round-trip audit**: withdrawals land at the hedge address and are
   swept into the vault via the existing receive/sweep path, making the
   flow event-auditable even though not Move-enforced.

If Move-level custody ever becomes a hard requirement, the only visible
paths are (a) Mysten adding cap-based ownership or app authorization for
MarginManagers, or (b) Bluefin adding on-chain account objects — worth
raising with both teams, not worth blocking on.

## Fit against the V1/V2 plan

- **Delta hedge**: both work. DBM = perfect hedge (actual spot short,
  zero basis) at an always-negative carry; Bluefin = proxy hedge with
  positive expected carry in bull tape (funding income is one of V1's
  four revenue lines) and much deeper leverage/liquidity.
- **Funding input to bids**: Bluefin has real hourly funding to feed
  the bid's hedge-cost term; DBM substitutes live borrow APR (simpler,
  always a cost).
- **Flash-exercise synergy**: our house DeepBook pools already expose
  zero-fee flash loans (`borrow_flashloan_base/quote`, repay exact
  amount, hot-potato). With DBM the whole §5 exercise chain — flash
  borrow strike → exercise → sell underlying → repay flash loan → repay
  margin debt (`place_market_order_and_repay_loan`) — is ONE atomic
  PTB. With Bluefin the hedge unwind is a separate async API call
  (hedge-lag risk the spec explicitly worries about).
- **Stress (+80% gap)**: a 5x DBM short liquidates on roughly a
  +18–25% move without top-ups (ratio 1.1 threshold) — the hedge margin
  buffer and flash-exercise pressure valve matter MORE on DBM than on
  Bluefin's deeper margin tiers. Run lower effective leverage (≤2x) on
  DBM.
- **Curator dashboard**: spot buys for the vault need no new venue —
  the deepbook-adapter's curator order functions already do it (UI +
  spot-pool allowlisting only). Hedge tab: DBM composes into the same
  wallet-signs-PTB stack via the official TS SDK; Bluefin needs their
  TS SDK + login flow against the hedge account.

## Sequencing (post-SO-334)

1. `HedgeVenue` trait + `paper` impl — DONE; the only venue the desk can
   execute against today.
2. Bluefin Phase-0 asks + a staging account (docs 03 §Phase 0) — the
   human-in-the-loop blocker.
3. `HedgeVenue::bluefin`: `bluefin-pro` Rust SDK, authorized-wallet
   trading, funding feed into the bid's hedge-cost term.
4. Curator dashboard: vault spot trading (existing adapter) + the
   Bluefin hedge panel, gated to the curator wallet.
