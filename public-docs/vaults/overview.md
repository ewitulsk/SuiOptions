---
description: "Curated trading vaults on Sui — pooled depositor capital, a curator who trades but can never withdraw, and the engine bootstrapping Pismo Options liquidity."
---

# Overview

Pismo Vaults are **curated trading vaults**: pooled capital that a designated **curator** actively trades across a set of integrated venues, under contracts that make one guarantee non-negotiable — **the curator can trade the money, but can never take it.**

A vault is not a strategy product with rules baked into code, and it is not a hedge fund running on legal trust. It sits deliberately between the two: the curator's *strategy* is discretionary, but the curator's *custody* is constrained by the Move type system. Every path by which value leaves the vault either returns it to the vault within the same transaction, pays a depositor's withdrawal, or passes through explicitly bounded, auditable channels.

## Why vaults exist

Pismo Vaults were built to solve the [options protocol's](../options/overview.md) bootstrap problem. An options market needs market makers; market makers need capital. Vaults crowdsource that capital: depositors pool funds, and the curator runs a market-making operation on top — quoting options, offloading inventory on [Pismo Exchange](../exchange/overview.md), executing spot on DeepBook, and managing exposure on external venues.

We are open about the current arrangement: **Pismo itself acts as curator of the flagship vault**, market-making the options protocol with community capital to bootstrap liquidity. We are actively looking to onboard a professional market-making firm to take over that role. The vault architecture is exactly what makes such a handover safe — the curator seat can change hands without depositors needing to trust the new occupant, because the occupant's powers are structural, not reputational.

## The three design principles

Everything in the vault design follows from three rules:

1. **The curator trades, and never withdraws.** No vault function pays out to whoever sent the transaction. Funds leave the vault's balances only into allowlisted venue integrations that must return everything to the vault in the same transaction — or through the narrow, capped, separately-documented external-account channel for venues that can't be custodied on-chain. See [The Curator Security Model](curator-security-model.md).
2. **Ledger shares, per-user cost basis.** Deposits mint non-transferable shares recorded in the vault's own ledger. Each depositor's cost basis is tracked individually, and the curator's performance fee is charged on *each user's own profit* at withdrawal — you never pay fees on gains you didn't experience.
3. **Oracle-free trading, oracle-priced accounting.** Nothing second-guesses the curator's trades — no price bands, no strategy checks. But every deposit and withdrawal is priced against a full, attested valuation of everything the vault holds, so nobody can enter or exit at a stale or manipulable share price. See [How Vaults Work](how-vaults-work.md).

## Open curation, marked trust

Vault creation is **permissionless** — anyone can create a vault and become its curator at launch. The Pismo app will clearly delineate between official **trusted curators** (vetted operators, including the Pismo-run vault) and **public vaults** (anyone else). The on-chain guarantees are identical for both; the label reflects operational vetting, not different contract rules. A public vault's curator can lose depositors' money through bad trading — no contract can prevent that — but they cannot steal it, and depositors should weigh the curator's identity accordingly. Read [For Depositors](for-depositors.md) before depositing into anything.

## What a vault can trade

A vault's capital can be deployed across four kinds of venue, each through its own audited integration:

* **Pismo Options** — the vault acts as a market maker, backing signed quotes directly with vault capital.
* **Pismo Exchange** — the vault quotes order books straight from its balances, no separate escrow needed.
* **DeepBook** — Sui's on-chain order book, for spot execution.
* **External venues** — capital sent, under strict on-chain budgets, to venues that can't be custodied on-chain (the Bluefin derivatives exchange is the first integration, with Aftermath Finance next). In the vault's role as an options market maker, these serve as **hedge venues** — places to lay off the risk the options book accumulates.

The venue system, and what keeps each one safe, is covered in [Venues](venues.md).

{% hint style="info" %}
Pismo Vaults currently run on Sui **testnet**, with mainnet in progress. Fee structure: *information coming soon* — the shape is a curator performance fee on each depositor's realized profit, with the protocol taking a cut of the curator's fee (never a separate charge on depositors), both within contract-enforced caps.
{% endhint %}
