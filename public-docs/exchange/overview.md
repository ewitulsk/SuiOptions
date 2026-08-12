---
description: "A hybrid exchange on Sui — free off-chain quoting for makers, on-chain settlement for everyone, and full compatibility with swap routers."
---

# Overview

Pismo Exchange is a spot exchange built on a simple observation: **no exchange on Sui offered free quoting while remaining compatible with swap routers.** Fully on-chain order books make every quote update a paid transaction, which punishes exactly the participants who create tight markets. Off-chain RFQ systems make quoting free but live outside the composable flow that routers and aggregators depend on. Pismo Exchange is built to be both.

## The hybrid model in one paragraph

Orders are **maker-signed messages**, not on-chain objects. Makers post, reprice, and cancel them over plain HTTP — free, instant, no gas. The order book and matching engine run off-chain. But every *fill* settles on-chain, in a settlement contract that independently re-verifies the maker's signature over the exact terms before a single token moves. The chain never trusts the exchange service; it trusts the maker's signature. And because the taker-side fill function is an ordinary composable Move call — coins in, coins out — any swap router can treat a Pismo Exchange order like one more liquidity source in a multi-hop route.

## Why it exists: the market-maker story

Pismo Exchange was built to support [Pismo Options](../options/overview.md). An options market maker continuously accumulates inventory — option tokens bought from traders, directional exposure from filled quotes — and needs to offload it to the open market. Doing that on a fully on-chain book means paying gas every time a quote moves, which for an actively-hedging maker is constantly.

On Pismo Exchange, a maker updates quotes by signing a new message. The cost of keeping a two-sided market fresh across every listed pair rounds to zero. That's what makes it economical to quote option tokens — assets whose fair price moves with every tick of the underlying — and it's why the exchange lists both ordinary asset pairs and option-token pairs.

The flywheel with the rest of the protocol: a [Pismo Vault](../vaults/overview.md) can quote the exchange **directly from vault capital** — the same pool that backs its options quotes — so inventory flows from options fills to exchange asks without capital ever fragmenting. See [Capital Efficiency](../capital-efficiency.md#one-capital-pool-two-venues).

## Why it matters for everyone else

* **Takers** get tighter prices, because makers who can requote freely can afford to quote tighter, and price-time priority matching means the best price wins.
* **Router users** hit exchange liquidity without knowing it exists. A route can split across Pismo Exchange orders and other venues in one atomic transaction, with a single end-of-route minimum-output check protecting the whole path.
* **Self-custody is preserved throughout.** Maker funds sit in a personal on-chain escrow account that only the settlement contract can debit — and only against that maker's own signature. Withdrawal is owner-only and instant, and deliberately keeps working even if the exchange is paused.

## What the exchange service is trusted for

Matching liveness and fairness — nothing else. The service cannot forge a fill, misprice a trade, or move funds: every fill requires the maker's signature over the exact terms, verified on-chain, and escrow only moves inside the settlement contract. If the service disappeared, resting orders could still be filled directly on-chain by anyone holding them, and every maker could withdraw instantly. The full breakdown is in [Limitations & Trust](limitations-and-trust.md).

{% hint style="info" %}
Pismo Exchange currently runs on Sui **testnet**, with a mainnet launch in progress. Trading fees: *information coming soon* — fees are charged only on fills, never on placing, updating, or cancelling orders.
{% endhint %}
