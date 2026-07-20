# Case study: how Ember integrates Bluefin

Status: research findings, 2026-07-20. Companion to `01-perps-venues.md`.
Ember (learn.ember.so) runs a structured/curated vault product on Sui very
similar in shape to our trading vaults, is Bluefin-incubated, and its
flagship "Basis Vault" explicitly shorts perps on Bluefin Pro — so it is
the closest live answer to our hedge-custody question.

Sources: Ember's published Move source
(github.com/ember-protocol/Ember-Vaults, incl. OtterSec + Asymptotic audit
PDFs), their production API (`vaults.api.sui-prod.bluefin.io`), Sui
mainnet RPC transaction forensics over their vault + sub-account
addresses, learn.ember.so / learn.bluefin.io docs, DefiLlama.

## TL;DR — they don't solve the custody problem; they route around it

Ember's on-chain vault has **zero Bluefin coupling** — no adapter module,
no venue dependencies at all (Move.toml deps are just std/sui). The
"integration" is:

1. A shared `Vault<T, R>` Move object doing **share accounting only**:
   deposit asset in, `Coin`-typed receipt tokens out (real coins — eSUI,
   eBASIS…), FIFO withdrawal queue, and an operator-posted exchange rate.
2. A privileged `operator` address calls
   `withdraw_from_vault_without_redeeming_shares(vault, sub_account,
   amount)` — a plain `public_transfer` of any amount to a whitelisted
   **sub-account EOA**. That is the entire venue surface.
3. The sub-accounts are **Fordefi MPC wallets** (named "Fordefi 1/2/3" in
   their own API), one strategy wallet per vault, spanning Sui, Ethereum,
   Solana, and even a Polymarket subaccount. These keypair wallets own
   the Bluefin Pro accounts: on-chain the ONLY Bluefin Pro function Ember
   sub-accounts ever call is `exchange::deposit_to_asset_bank` crediting
   **their own address** (verified across ~3,500 sub-account txs; no
   `authorize_account`, no old-v2 MarginBank, no delegation events).
   All perp trading is off-chain via Bluefin's sequencer.
4. **NAV is operator-attested, not computed on-chain**: a `rate_manager`
   address posts a new exchange rate (typically Tue+Fri; one daily tx
   updates all vaults), with contract-enforced guardrails — max rate
   change per update (1% on the Basis vault), minimum update interval
   (24h), and protocol-wide min/max bounds. Their auditor (Asymptotic)
   states it plainly: *"Ember Vaults operates as a managed fund … The
   protocol enforces share accounting, FIFO withdrawal ordering, and
   rate change limits on-chain. It cannot verify fund deployment or
   validate rate accuracy."*
5. Withdrawals: users enqueue receipt tokens; the operator later
   processes the queue **at the processing-time rate**, within a
   per-vault withdrawal period (2–14 days typical, up to quarterly for
   RWA vaults). Nothing enforces processing timeliness; if the operator
   halts, redemptions halt.
6. Transparency is **self-reported**: a "Proof of Capital" API breaks
   NAV down per wallet/protocol (their Basis vault showed ~$654K of
   ~$2.3M on `bluefin`), but it's their own backend reporting, not
   attestation.

The "deep Bluefin integration" is corporate, not cryptographic: Ember is
incubated by Bluewater Labs (the Bluefin team, with Upshift), its API and
app run on `*.bluefin.io` domains, and Bluefin migrated its legacy BLUE
staking vaults into Ember — but on-chain, Ember uses exactly the same
public `deposit_to_asset_bank` anyone can call, with no special roles.

Operationally on Sui: 29 vaults share **one operator address and one
admin address** (Ember/Bluefin ops), with role separation asserted
on-chain (admin ≠ operator ≠ rate_manager ≠ sub_account). ~$126M TVL
across Pharos/Ethereum/Sui as of this research; institutional depositors
(SUI Group seeded $10M in Feb 2026).

## What this confirms and what it changes for us

**Confirms our venue analysis.** Even the Bluefin-incubated team could
not put a Bluefin Pro account under Move-object custody — because it
isn't possible (keypair-backed accounts, sequencer-mediated
withdrawals). They chose the managed-fund model: MPC key custody +
whitelists + bounded rate updates + self-reported transparency.

**Our vault is strictly stronger on-chain, by design.** Ember's operator
can transfer *any amount* out of the vault to a whitelisted wallet; our
curator provably cannot withdraw at all, NAV is computed on-chain from
oracle attestations at every deposit/fulfillment (not posted), and
custody of on-chain venues (DeepBook, our options, DBM lending) is
Move-enforced. Ember traded all of that away to get unlimited venue
reach (CeFi, RWA, Polymarket, cross-chain). These are two coherent but
different products; we should not drift into their trust model by
accident.

**The pattern worth adopting — a bounded escape hatch, not an open
door.** If/when we add a Bluefin hedge leg, the Ember comparison
suggests doing what they did but with the two on-chain controls they
lack:

1. **Budgeted, allowlisted external-venue releases**: a vault function
   that transfers to an admin-allowlisted hedge address only, capped as
   a % of NAV with a rate limit (e.g. X%/day) — vs Ember's uncapped
   operator withdrawal. The vault then has a Move-enforced maximum
   external exposure instead of a trusted operator.
2. **Appraised external exposure**: the released amount is tracked as a
   vault "external position" appraised via an oracle-adapter attestation
   (operator/keeper-posted equity with Ember-style guardrails — max
   delta per update + min interval — enforced by the attestation
   consumer), so NAV degradation is bounded and visible rather than
   silently baked into a posted rate.

Plus the organizational pieces Ember validates: MPC (Fordefi-style)
custody for the hedge key rather than a raw keypair, per-vault strategy
wallets, role separation, and a public proof-of-capital style
reconciliation report.

**Other details worth noting.**
- Their receipt tokens are transferable `Coin`s, which buys composability
  (Bluefin Lend accepts them at 80% LTV) but forecloses per-user
  cost-basis performance fees — exactly the trade we made in the other
  direction (ledger stakes) on purpose.
- Withdrawal crystallization at processing-time rate matches our
  fulfillment-time crystallization.
- Their rate-guardrail shape (max_rate_change_per_update + min interval
  + global bounds) is a good concrete spec for any operator-attested
  input we ever accept into NAV.
- OtterSec flagged (and they fixed) an operator-blacklist-after-request
  griefing path — a reminder to audit our own force/closure paths for
  the same class of issue.
