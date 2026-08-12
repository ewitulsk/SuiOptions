---
description: "American-style, fully collateralized calls and puts on Sui — options that live in your wallet as ordinary coins."
---

# Overview

Pismo Options is a protocol for writing, trading, and exercising **American-style options** on Sui. It supports both sides of the classic pair:

* **Covered calls** — collateralized 1:1 by the underlying asset. Exercising pays the strike price in the settlement asset and receives the underlying.
* **Cash-secured puts** — collateralized by the full cash value at the strike. Exercising delivers the underlying and receives the strike price in cash.

"American-style" means a holder can exercise **any amount, at any time before expiry** — no waiting for a settlement date, no cash-settled approximation. Every exercise is physical: real assets move.

## What makes it different

Three design decisions separate Pismo Options from other on-chain options designs.

### 1. Every option is fully collateralized. Always.

There is no margin, no partial collateral, no liquidation engine, and no insurance fund. When a call is written, the full underlying is locked in the contract. When a put is written, the full strike value in cash is locked — rounded *up*, so the pool can always pay out. An option holder never faces counterparty risk: the assets backing their option are provably on-chain from the moment the option exists until it is exercised or expires.

This is a deliberate trade against capital efficiency — and the protocol claws that efficiency back elsewhere, through [offset closure and spread compression](../capital-efficiency.md#netting-options-positions-early), rather than by thinning the collateral guarantee.

### 2. Options are ordinary coins

Each option series is a real fungible Sui coin with its own currency type. Your wallet shows it as a balance. You can split it, combine it, transfer it, sell it on [Pismo Exchange](../exchange/overview.md), or send it to a friend — it behaves like any other coin, because it *is* one.

The supply is honest by construction: option coins are minted only when collateral enters the pool and burned on exercise or after expiry, through a mint authority the pool alone controls. Outstanding coin supply always equals outstanding options, and the type system guarantees a coin can only ever be redeemed against the exact pool that minted it.

### 3. Prices come from competing market makers, not a formula

Most on-chain options protocols price with an AMM curve or an oracle-fed formula, which tends to mean wide spreads and stale vol. Pismo Options is **quote-driven**: when you want to trade, your request is broadcast to every connected market maker, they respond within seconds with signed, executable prices, and you pick the best one. The quote you accept is embedded in your transaction and re-verified on-chain — the price you saw is exactly the price you get, or the transaction doesn't execute at all.

Because [quoting costs makers nothing](../capital-efficiency.md#quoting-is-free-everywhere), makers can afford to reprice constantly and quote tight.

## The pooled-bucket model, in one paragraph

All writers of the same contract — same underlying, expiry, strike, and settlement asset — share a single on-chain pool called a **bucket**. Exercises are assigned to writers strictly first-in-first-out, tracked by a single cursor that advances in constant time no matter how many writers share the pool. Early writers wrote when the option was cheaper and received smaller premiums; they stand first in the exercise queue. Late writers were paid more and stand behind them. Exercise risk always corresponds to the premium you were paid — and assignment is a deterministic function of on-chain counters, so there is no assignment lottery and nothing for an operator to manipulate. The full mechanics are in [Buckets & Assignment](buckets-and-assignment.md).

## Who does what

| Role | What they do | Where to read more |
|---|---|---|
| **Writers** | Deposit collateral, receive premium instantly. After expiry, redeem for collateral back and/or exercise proceeds. | [Trading Options](trading.md) |
| **Buyers** | Pay premium, receive option coins. Exercise any time before expiry, or sell the coins on. | [Trading Options](trading.md) |
| **Market makers** | Quote both sides continuously via signed off-chain quotes, backed by their own collateral account or by a [Pismo Vault](../vaults/overview.md). | [Market Making](market-making.md) |
| **The protocol** | Lists new expiries and strikes on a rolling schedule; skims a fee from premiums. Fee details: *information coming soon*. | [Limitations & Trust](limitations-and-trust.md) |

{% hint style="info" %}
Pismo Options currently runs on Sui **testnet**, with a mainnet launch in progress.
{% endhint %}
