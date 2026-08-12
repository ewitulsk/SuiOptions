---
description: "What the chain enforces, what off-chain services can and cannot do, and the honest limitations of Pismo Options today."
---

# Limitations & Trust

This page separates what is **cryptographically enforced** from what is **operationally provided** — and states plainly where the protocol's current limits are.

## What the chain enforces

These invariants hold no matter what any server, admin, or market maker does:

* **Full collateralization.** Every call is backed 1:1 by underlying locked in its bucket; every put by the full strike value in cash, rounded in the pool's favor. There is no undercollateralized state to reach.
* **Honest supply.** Option coins are minted only when collateral enters and burned on exercise or expiry, through a mint authority the bucket alone controls. Outstanding supply always equals outstanding options.
* **Deterministic assignment.** Exercise assignment is a pure function of two on-chain counters. No operator chooses who gets assigned, and no assignment can be reordered after the fact.
* **Quote integrity.** Trades execute only against a maker's signed quote, re-verified on-chain for signature, expiry, and single-use nonce. A quote cannot be replayed, altered, or forged — and the funding route is inside the signature, so proceeds cannot be redirected by anyone.
* **Atomicity.** A trade either fully executes — collateral in, premium routed, position and option coins minted, fee skimmed — or fully reverts. There are no partial states.

## What off-chain services can and cannot do

| Service | Holds funds? | Worst case if compromised or offline |
|---|---|---|
| **Quoting service** | No | Withhold, censor, or reorder quotes. It cannot alter a price, size, or route — those are inside the maker's signature — and it cannot move funds. If it's down, trading pauses; existing positions are unaffected. |
| **Scheduler** (bucket listing) | Admin capability | List misconfigured buckets or stop listing new ones. It cannot touch funds inside existing buckets. |
| **Indexer / API** | No | Serve stale or wrong data to the app. On-chain state remains the truth. |

Market makers are treated as **untrusted**. A maker who signs quotes they can't back causes a failed (fully reverted) transaction — the on-chain check is the safety net. The quoting service additionally tracks maker reliability and deprioritizes repeat offenders.

## Admin powers

The protocol admin can: list and pause buckets, set the protocol fee within a hard-coded contract maximum, and withdraw accumulated **protocol fees**. The admin cannot withdraw user funds from buckets or positions, cannot mint options, and cannot influence assignment. Pausing a bucket blocks *new writes only* — exercises and redemptions always remain open, so funds already in the system can always come out.

## Honest limitations

* **Not on mainnet yet.** Pismo Options runs on Sui testnet today. Mainnet is being actively worked toward.
* **Liquidity depends on connected makers.** Quote-driven pricing means no makers online → no firm prices. During bootstrap, much of the quoting is done by the Pismo-curated vault (see [Pismo Vaults](../vaults/overview.md)); we're transparent that early liquidity is concentrated and are working to onboard independent market-making firms.
* **Reverted fills are possible.** Because maker collateral isn't locked per quote, an over-committed maker causes your fill to revert (costing you gas, nothing more). Reputation-based sorting mitigates this today; a stronger reputation system is planned.
* **Full collateralization caps capital efficiency.** A written option locks real collateral. Offset closure and spread compression recover much of this, but the protocol will never offer margined option writing — that's a feature, and it's also a limitation worth understanding.
* **Physical settlement requires action.** In-the-money options are not auto-exercised at expiry. Exercising is the holder's responsibility; an unexercised in-the-money option expires worthless.
* **Buckets are shared queues.** You cannot choose your assignment position except by when you write. Writing early in a bucket's life means lower premiums and earlier assignment; late means the reverse. This is disclosed, deterministic, and fair — but it is not the isolated-position model some traders expect.
