---
description: "Fully collateralized options, a hybrid exchange, and curated liquidity vaults on Sui — three products, one liquidity flywheel."
---

# Pismo Protocol

Pismo Protocol is a suite of three trading products on the Sui blockchain:

| Product | What it is |
|---|---|
| [**Pismo Options**](options/overview.md) | American-style, fully collateralized calls and puts. Every option is an ordinary Sui coin, priced by competing market makers and settled atomically on-chain. |
| [**Pismo Exchange**](exchange/overview.md) | A hybrid spot exchange: order books live off-chain so quoting is completely free, but every fill settles on-chain and composes with swap routers like any AMM. |
| [**Pismo Vaults**](vaults/overview.md) | Curated trading vaults. Depositors pool capital; a curator trades it across integrated venues but can never withdraw it. |

## Why three products?

The three products aren't a grab bag — they are one system, built in a deliberate order to solve one problem.

**The options protocol is the flagship.** It gives Sui something it doesn't otherwise have: fully collateralized, exercisable options that live in your wallet as plain coins. But an options market is only as good as its liquidity, and options liquidity comes from market makers — professionals who continuously quote both sides and manage the risk they accumulate.

That creates two bootstrapping problems, and the other two products each solve one:

1. **Market makers need capital.** [Pismo Vaults](vaults/overview.md) crowdsource it. Anyone can deposit into a vault; the vault's curator trades that capital — including acting as a market maker on the options protocol — while the contracts guarantee the curator can never move funds to themselves. Pismo runs the initial vault as curator to bootstrap options liquidity with community capital, and we're actively looking to onboard a professional market-making firm to take over that role.
2. **Market makers need somewhere to offload risk.** A maker who buys option flow accumulates inventory and directional exposure. They need venues to lay that risk off — selling option tokens to the open market, hedging spot exposure. [Pismo Exchange](exchange/overview.md) exists because no exchange on Sui offered what a market maker actually needs: **free quoting** (updating prices costs nothing, no transaction required) while remaining **compatible with swap routers**, so retail flow from aggregators can still hit those quotes. Vaults also integrate external venues — DeepBook for spot, and derivatives venues like Bluefin — as additional places to manage exposure.

The result is a flywheel: vaults supply the capital, the exchange and integrated venues supply the risk outlets, and both feed tighter, deeper markets on the options protocol.

## The design philosophy

Two ideas repeat across everything Pismo builds:

**Capital efficiency.** Quoting is free on both the options protocol and the exchange — a market maker signs prices off-chain and pays nothing until a trade actually happens. The same pool of vault capital can back quotes on both systems simultaneously. Options positions can be netted and compressed to free collateral early. The full story is on the [Capital Efficiency](capital-efficiency.md) page.

**Services are trusted for liveness, never for funds.** Pismo runs off-chain infrastructure — quote routing, order matching, maintenance cranks. None of it can move user funds, forge a trade, or change a price. Every fill requires a signature from the party whose funds move, verified on-chain. If every Pismo server disappeared tomorrow, funds would remain recoverable through permissionless on-chain paths. The [Protocol Infrastructure](infrastructure/architecture.md) section spells out exactly what each service can and cannot do, and each product has its own [Limitations & Trust](options/limitations-and-trust.md) page that we've tried to make honest rather than reassuring.

{% hint style="info" %}
Pismo Protocol currently runs on Sui **testnet**. We are actively working toward a mainnet launch.
{% endhint %}

## Where to start

* **Trade options** — start with the [Pismo Options overview](options/overview.md), then [Buckets & Assignment](options/buckets-and-assignment.md) for the core model.
* **Earn as a depositor** — read [Pismo Vaults](vaults/overview.md) and, before depositing anything, [For Depositors](vaults/for-depositors.md).
* **Market-make or run a vault** — [Market Making](options/market-making.md), [Capital Efficiency](capital-efficiency.md), and [The Curator Security Model](vaults/curator-security-model.md).
* **Understand the trust model** — [Protocol Infrastructure](infrastructure/architecture.md).
