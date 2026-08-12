---
description: "How prices and external values enter Pismo Protocol — allowlisted attestations, per-asset pinning, and guardrails on every operator-posted input."
---

# Oracles & Attestations

Pismo Options needs no oracle to trade — quotes are market-made, settlement is physical, and strike math is exact integer arithmetic. Oracles enter the picture where **valuation** does: pricing vault shares, marking option positions, and accounting for capital at external venues. This page explains how those inputs are controlled.

## The attestation model

Nothing in the vault system consumes a raw price. It consumes a **price attestation** — a short-lived, in-transaction proof that a specific oracle adapter vouched for a specific asset's price at a specific moment. Attestations cannot be stored or reused; they exist only inside the transaction that mints them.

Three controls govern who can mint them:

1. **Allowlist.** Only oracle adapters approved by protocol governance can produce attestations at all. Publishing a new adapter does nothing until it's allowlisted; delisting one instantly stops its attestations — an immediate kill switch that never strands funds, because the vault's exit paths don't require prices.
2. **Per-asset pinning.** Governance can pin a specific adapter per asset, so running two oracle providers in parallel is *safe* rather than merely possible — a second provider being allowlisted doesn't grant it authority over assets pinned to the first, and migrations can proceed asset by asset.
3. **In-adapter guardrails.** Each adapter enforces freshness (a maximum age on the underlying feed data), sanity bounds on magnitudes, and — where the upstream provides it — confidence checks that reject wide-uncertainty prints. These limits live in governance-controlled registry state, not in caller arguments, so no transaction can loosen them.

Two independent price networks are integrated as adapters — Pyth and Switchboard — with the pinning system managing which serves which asset.

## Derived marks: option values

Vault-held option positions are valued by a dedicated adapter that composes spot attestations into option marks. Its design goal is *conservative, manipulation-resistant, never wedged*:

* **Intrinsic value** is computed from attested spot prices and the contract's strike — exact and always available.
* **Time value** is added from a protocol-maintained realized-volatility book: a simple, bounded at-the-money estimate, decayed away from the money and hard-capped at the no-arbitrage bound. Never a market quote (order books can be painted), never an unbounded model.
* If the volatility input is stale or missing, marks **degrade to intrinsic value** rather than blocking — a vault can always be appraised, just more conservatively.
* Expired options mark at dust.

The result deliberately *understates* option value in most states. Undercounting is the safe direction: it can delay recognizing profit; it can never mint shares against value that isn't there.

## Operator-posted inputs: the guardrail pattern

Two inputs cannot come from a price network at all: **realized volatility** (computed from history) and **external-venue account equity** (read from a venue's account API). Both are posted on-chain by the keeper — which makes them operator inputs, and operator inputs get the same four-part on-chain guardrail wherever they appear:

| Guardrail | What it prevents |
|---|---|
| **Poster allowlist** | Anyone but approved posters writing the value |
| **Minimum update interval** | A compromised poster rapid-firing updates |
| **Maximum change per update** | Any single update moving the value far — combined with the interval, total drift speed is hard-bounded |
| **Staleness limit** | Old values being silently relied on — a stale entry stops flows rather than mispricing them |

Correcting a genuinely diverged value (say, after a venue moves sharply while posting was down) requires an explicit governance re-anchor — a deliberate, logged act, not something a poster key can do. And initializing an external account's equity entry at **zero** is permissionless, because zero is the one value the chain can already prove.

The consequence of the guardrails, stated as a bound: a fully compromised poster key cannot teleport a value — it can only *walk* it, at a capped percentage per capped interval, while reconciliation monitoring compares posted equity against recorded deployments and raises alerts on divergence. Slowing an attacker to a crawl inside an alarmed corridor is the design.

## What we don't claim

Oracle risk is not eliminated; it is bounded, monitored, and pointed in the conservative direction. Attested prices can be briefly wrong within their freshness and confidence windows; realized-vol marks are estimates by construction; venue equity is ultimately a venue-reported number passing through guardrails. The system is designed so that each of these failing costs *accuracy* — slightly mispriced entries or delayed profit recognition — rather than *custody*: no oracle input, corrupted or missing, creates a path for funds to leave the protocol.
