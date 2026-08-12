---
description: "The off-chain services behind Pismo Protocol, and the principle that governs all of them — trusted for liveness, never for funds."
---

# Architecture

Pismo Protocol is a set of Move contracts on Sui plus a small fleet of off-chain services. This page is the cross-cutting map: what runs off-chain, why it exists, and the one principle that governs the whole design.

## The principle: liveness, never funds

Every off-chain service in Pismo is built to the same standard: **it may be trusted to keep things running, never to keep things honest.** Honesty — who owns what, what price a trade executes at, where funds move — is enforced by contracts verifying signatures from the parties whose funds are at stake. Consequences of the principle:

* No service custodies user funds. The most any service holds is a gas wallet, or one *share* of a jointly-controlled key that can only sign policy-bounded shapes.
* No service's signature can move your assets. Fills require the maker's signature; releases require the contracts' checks; vault flows require attested appraisals.
* Every critical *recovery* path is permissionless: exercising options, redeeming positions, withdrawing exchange escrow, force-unwinding vaults, fulfilling withdrawal queues. Services make these things convenient; they never make them possible.

The design was chosen so that this sentence is true: **if every Pismo server vanished, no user would lose custody of anything** — trading would pause, and funds would come out through permissionless paths.

## The services

### Quoting service *(Pismo Options)*

A WebSocket router between traders and market makers. Broadcasts quote requests, validates and sorts the signed quotes that come back, and tracks maker reliability. Holds no funds and no keys; performs no balance checks (fill-time atomicity on-chain is the enforcement). A compromise can censor or reorder quotes — never alter one, because every economic term is inside the maker's signature. Details: [Market Making](../options/market-making.md#what-the-quoting-service-does-and-doesnt-do).

### Order book service *(Pismo Exchange)*

Maintains the off-chain books, matches by price-time priority, submits matched settlements through a relayer key, and mirrors on-chain events back into its state. Trusted for matching liveness and fairness only: every fill needs maker signatures verified on-chain, fill accounting is on-chain, and the on-chain event stream is the source of truth the service reconciles *itself* against. Details: [Exchange — Limitations & Trust](../exchange/limitations-and-trust.md).

### Keeper *(Pismo Vaults)*

The maintenance crank: valuation refreshes, expired-position redemption, proceeds sweeps, withdrawal fulfillment, force-unwinds, and guardrailed oracle posting. Holds only a gas wallet — no admin capability, no curator capability — and everything it does is contract-validated and callable by anyone. Multiple keepers can run concurrently; anyone can operate one. Details: [Vaults — Limitations & Trust](../vaults/limitations-and-trust.md#the-services-around-vaults).

### Signing service *(Pismo Vaults, external venues)*

Holds the protocol's half of each vault's jointly-controlled external-venue key, plus the registrar key that attests self-serve external-account registrations. Co-signs only a fixed set of policy-checked shapes — venue login, curator-key authorization, deposits that credit the joint account, withdrawals that (by venue design) can only pay the joint account, and sweeps that only pay the vault. Fail-closed on anything unrecognized; every decision append-only logged. Details: [Venues — External venues](../vaults/venues.md#external-venues).

### Scheduler *(Pismo Options)*

Lists new option buckets on a rolling schedule — fresh expiries, volatility-aware strike grids. Holds the admin capability for listing; cannot touch funds in existing buckets.

### Indexer & app services

Read-side plumbing: chain event indexing, market data, the web app's APIs, gas sponsorship for app-built transactions. None hold user funds; all can be wrong only in what they *display*, with the chain as the correctable truth.

## Failure modes at a glance

| If this dies… | What stops | What never stops |
|---|---|---|
| Quoting service | New options quotes | Exercise, redemption, transfers, everything on-chain |
| Order book service | New matching; the exchange app | Direct on-chain fills of resting orders, escrow withdrawals, all settled history |
| Keeper | Convenience: fresh marks, automatic fulfillment | The same cranks run by anyone else; grace-period force-unwind |
| Signing service | External-venue boundary operations | All on-chain vault activity; curator's venue trading; every exit that doesn't cross the external boundary |
| Scheduler | New bucket listings | Every existing bucket's full lifecycle |

A deliberate omission from this page: Pismo also operates its own market-making systems (as the initial [vault curator](../vaults/overview.md)). Those are a *user* of the protocol — the same signed-quote, same-capital-rules user any third-party maker would be — not part of its infrastructure, which is exactly the point.
