---
description: "What the vault contracts enforce, which services keep vaults running, and what is detected rather than prevented."
---

# Limitations & Trust

## What the chain enforces

* **No curator withdrawal path.** No vault function pays vault funds to the transaction sender or to any curator-chosen address. Trading sessions must balance and close within their transaction; venue positions are custodied inside the vault; only allowlisted integrations can act on them, and their exits pay the vault.
* **Bounded external channel.** The single external account is destination-fixed, budget-capped as a fraction of true (same-transaction-appraised) vault value, rate-limited daily, and — when self-served by a curator — registerable only once, only with proof of protocol key co-custody, at conservative caps.
* **Honest share pricing.** Deposits and withdrawal batches only execute against complete, fresh appraisals; anything moving mid-valuation aborts the transaction. No donation path exists; share-inflation attacks are unprofitable by construction.
* **Permissionless exits.** Withdrawal fulfillment, force-unwind after the grace period, appraisal cranks, sweep cranks, and final closure are all callable by anyone. No depositor outcome depends on the curator's — or Pismo's — continued participation.

## The services around vaults

Two Pismo-operated services matter to vault operation. Neither holds vault funds.

**The keeper** is the maintenance crank: it refreshes valuations, redeems expired option positions, sweeps venue proceeds into vault custody, fulfills withdrawal queues, force-unwinds starved vaults, and posts the guardrailed volatility and external-equity marks. It holds no capability objects — just a gas wallet — and every action it takes is validated by the contracts and callable by anyone else. If Pismo's keeper died, a community keeper could replace it the same day, and the [permissionless escape hatches](how-vaults-work.md#the-liveness-backstop) don't even need that.

**The signing service** (for external venues) holds the protocol's half of each vault's joint venue key and a fail-closed policy for what it will co-sign. Its compromise cannot steal vault funds — the co-signable shapes only move money between the vault, the joint account, and the venue — but its *unavailability* freezes the external channel's boundaries: no new venue deposits, withdrawals, or sweeps until restored (curator trading at the venue continues unaffected, and so does everything on-chain). Every signing decision is logged append-only.

| Service | Holds funds? | Worst case if compromised |
|---|---|---|
| Keeper | Gas wallet only | Stall maintenance until someone else cranks it; post equity/vol marks only within on-chain rate/delta guardrails |
| Signing service | No (key share only) | Refuse to co-sign (freezing external-channel boundaries); cannot redirect funds anywhere the policy shapes don't allow |
| Indexer / app | No | Show stale data; on-chain state remains the truth |

## Governance powers

Protocol governance (the admin capability) can: allowlist and delist venue integrations and oracle adapters, register external accounts above the self-serve caps, correct a diverged equity mark (a logged, deliberate act), set protocol fee parameters within contract caps, and pause new vault activity. It cannot withdraw vault funds, cannot override the appraisal requirements, and cannot block the permissionless exit paths — delisting an integration stops new deployment through it while its force-unwind exits remain open by design.

## What is detected, not prevented

Honesty about the vault design means being precise about which protections are *walls* and which are *alarms*:

* **Curator trading losses** — including deliberate ones — are not prevented (see [the security model](curator-security-model.md#what-this-model-does-not-prevent)). They are bounded in speed (external budget and rate caps), surfaced quickly (conservative marks, continuous appraisals), and monitored (reconciliation alerts between deployed capital and attested venue equity). The internal shorthand is blunt: *adversarial trading is slow withdrawal* — the caps make it slow, the monitoring makes it loud, and the trusted-curator vetting exists to make it unlikely.
* **External venue failure** is outside the protocol's control entirely; the budget cap is the only bound.

## Honest limitations

* **Not on mainnet yet.** Testnet today; mainnet in progress.
* **The external-venue integration is early.** The Bluefin path currently operates only in staging environments and will be hardened before mainnet vaults can enable it. Vaults work fully without it — options, exchange, and DeepBook custody are entirely on-chain.
* **Valuations lean on oracles.** Share pricing is only as good as the allowlisted, guardrailed oracle attestations behind it (see [Oracles & Attestations](../infrastructure/oracles.md)). Guardrails bound how fast a bad input can move anything, but oracle risk is real and we don't pretend otherwise.
* **Value briefly in flight.** Between a vault-funded options fill and the sweep that follows, proceeds sit at the vault's address awaiting custody — invisible to valuation for that window. The effect is a transient, bounded *understatement* of vault value (never an overstatement), resolved by the next sweep crank.
* **Curator liveness matters for performance.** An absent curator can't lose your money, but a vault with no active curator earns nothing while capital sits idle — until depositors withdraw or the vault is closed.
