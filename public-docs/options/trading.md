---
description: "How a trade happens on Pismo Options — request quotes, pick the best signed price, and settle atomically in a single transaction."
---

# Trading Options

Pismo Options is quote-driven: prices come from professional market makers competing to fill your request, and every fill settles atomically on-chain. There is no order book to sit on and no partial-fill risk — either the whole trade executes at the quoted price, or nothing happens.

## The quote flow

1. **Pick a contract.** Browse the live option chain — each row is a bucket: an (asset, expiry, strike) contract, for calls or puts. The indicative prices you see on the chain are lightweight, non-binding quotes averaged across makers and refreshed continuously.
2. **Request firm quotes.** When you choose a size and side, the app sends a quote request to the quoting service, which broadcasts it to every connected market maker on the other side of your trade.
3. **Makers respond within seconds** with signed, executable quotes. The service validates each one — signature, expiry, correct contract and size — and returns the surviving quotes sorted best-price-first, tagged with each maker's fill-reliability score.
4. **You pick a quote and sign one transaction.** The maker's signed quote is embedded in your transaction. The chain independently re-verifies everything — the signature, that the quote hasn't expired, that it has never been used before — and executes the entire trade in one atomic step.

The maker is not involved at fill time at all. They don't co-sign, don't submit anything, and don't even need to be online — you hold their signed promise, and the chain enforces it.

## What's inside a quote

Every quote is signed by the market maker's registered on-chain key and covers, among other fields:

* the exact **bucket, size, and premium** — nothing about the trade can be altered after signing,
* an **expiry** (typically under a minute) — stale quotes cannot execute,
* a **single-use nonce** — the first execution permanently burns it on-chain; replays are impossible,
* a **protocol identifier** unique to this deployment — a quote signed for one network can never replay on another,
* the **funding source** — which collateral account or vault will pay the maker's side. Because this is *inside* the signature, no intermediary (including Pismo's own services) can redirect where the maker's funds come from or where their proceeds go.

## Writing (earning premium)

You deposit collateral and immediately receive the premium — paid by the market maker who bought your option — minus the protocol fee (*fee details: information coming soon*).

* **Writing a call:** you deposit the underlying asset. You receive a **Position** object recording your range in the FIFO queue, plus the premium in the settlement asset, instantly. The market maker receives the freshly minted option coins.
* **Writing a put:** identical, except your collateral is the settlement asset — the full strike value of what you might be assigned.

After expiry, you redeem your Position for your FIFO outcome: unexercised collateral back, and exercise proceeds for any exercised portion (strike cash for calls; delivered underlying for puts). See [Buckets & Assignment](buckets-and-assignment.md).

Positions are transferable objects — you can move or sell the writer side of a trade as well.

## Buying (paying premium)

You pay the premium; the market maker's funding source provides the full collateral that backs the option. The option coins land directly in your wallet, fungible with every other option of the same series.

From there you can:

* **Exercise** — any amount, any time before expiry. For a call: pay `amount × strike` in the settlement asset, receive the underlying. For a put: deliver the underlying, receive `amount × strike` in cash. Partial exercise is just a coin split.
* **Sell** — option coins are ordinary Sui coins; the natural venue is [Pismo Exchange](../exchange/overview.md), where makers quote option tokens against cash.
* **Hold to expiry** — an option that finishes out of the money simply expires; the coins become worthless and can be burned.

## Atomicity and what can go wrong

The entire fill — your funds in, the maker's funds released, position minted, option coins minted, fee skimmed — happens in one transaction. There are no partial states.

The one failure mode to know about: makers' collateral is **not locked per quote** (see [Capital Efficiency](../capital-efficiency.md#no-locked-collateral-per-quote)). If a maker signed more than their account can currently cover, your transaction **reverts harmlessly** — you lose a little gas, nothing else, and the quote's nonce is not consumed by a failed attempt. The quoting service tracks each maker's revert rate and deprioritizes unreliable makers; a fuller maker-reputation system is planned.
