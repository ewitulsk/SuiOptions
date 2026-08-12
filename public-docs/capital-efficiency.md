---
description: "Free quoting, one capital pool across two venues, and position netting — how Pismo minimizes the capital a market maker must lock up."
---

# Capital Efficiency

Capital efficiency is the design goal that ties Pismo's products together. Market making is a margin business: the less capital a maker must lock up per unit of liquidity provided, and the less it costs to keep quotes fresh, the tighter the spreads everyone else gets to trade against. This page collects the mechanisms — spread across the options protocol, the exchange, and the vaults — that exist for exactly that reason.

## Quoting is free. Everywhere.

On both Pismo Options and Pismo Exchange, a quote is an **off-chain signed message**, not an on-chain object.

* On the **options protocol**, a market maker receives a quote request over WebSocket, prices it, signs the quote bytes, and sends them back. No transaction, no gas, no on-chain state. A quote that goes unused simply expires — there is nothing to cancel.
* On the **exchange**, an order is a signed message posted to the order book service over HTTP. Placing, repricing, and cancelling are all free. The chain is touched only when a fill actually happens.

This inverts the cost model of fully on-chain order books, where every price update is a transaction. A maker on Pismo can reprice continuously — hundreds of times a minute if they like — at zero cost. Stale quotes are the largest hidden cost in on-chain market making; making requotes free is the single biggest efficiency win in the system.

## One signature pattern, verified on-chain

Free quoting is only safe because of how fills work. Both systems use the same architecture:

1. The maker signs the **exact economic terms** — asset, amount, price, expiry, a replay-protection nonce or salt, and *where the funds will come from*.
2. At fill time, the chain re-verifies that signature and executes atomically. Either the entire trade happens at the signed terms, or nothing happens at all.
3. The funding source named in the signature is asked to release exactly the required amount, inside the same transaction.

No off-chain service sits in the trust path. The quote router and the order book can go down, censor, or misbehave — they can never alter a price, redirect funds, or execute anything the maker didn't sign.

## No locked collateral per quote

Neither system reserves or locks collateral when a quote is outstanding. A maker's capital sits in one pot — a collateral account, an exchange escrow balance, or a vault — and every outstanding quote is a claim against that same pot.

The enforcement is at fill time: if the pot can't cover a fill, the transaction **reverts atomically**. Nothing partial happens, the maker loses nothing, and the taker loses only gas. This means a maker can have far more quoted size outstanding than capital deposited — quoting a full options chain and a full order book ladder from one balance — and capital is only consumed by trades that actually execute.

The trade-off is honest to state: an over-committed maker causes failed transactions for takers. Today the quote router tracks each maker's fill and revert rates and sorts their quotes accordingly; a maker whose quotes revert gets deprioritized. We plan to extend this into a fuller **market-maker reputation system**, where failed user transactions cost the responsible maker standing — an important guardrail as automated retail strategies begin quoting through vaults.

## One capital pool, two venues

This is the piece that makes the whole system compound. A Pismo Vault can act as the funding source for **both** systems simultaneously:

* The vault implements the options protocol's collateral-release interface, so a curator's market-making bot can sign options quotes backed directly by vault funds. The contracts force every coin the maker side receives — premiums, option tokens, returned collateral — to land back in the vault.
* The vault plugs into the exchange through **direct escrow**: the vault's free balances *are* the escrow behind its signed exchange orders. No capital is moved into a separate exchange account; a fill settles straight out of, and into, the vault.

The same dollar of vault capital can therefore back an options quote and an exchange order at the same time. When an options fill lands the vault with option tokens, the curator can immediately quote them for sale on the exchange — offloading inventory to the open market and to swap-router flow — without capital ever being fragmented across venue-specific accounts.

For a sophisticated market maker this double-commitment is a feature: it is exactly how a professional desk thinks about one balance sheet quoting many venues. The revert-on-insufficient-funds guarantee means it can never create losses — only failed fills — and the reputation system is the long-term mechanism for keeping those failures rare.

## Netting options positions early

Fully collateralized options are safe but capital-hungry: a written call locks the full underlying until expiry. Pismo Options ships two primitives that release that collateral as soon as it is economically redundant:

### Offset closure

A maker who has *written* options in a bucket and later *buys back* the same option holds two positions that cancel economically. **Offset closure** lets them net on-chain: burn the option coins against the written position, and withdraw the freed collateral immediately — no waiting for expiry. The bucket's assignment queue simply skips over the closed range.

This is what makes inventory recycling work: write a call, buy it back cheaper on the exchange, net the two, and redeploy the collateral — all mid-cycle.

### Spread compression

A maker holding a long call can write a call at an equal or higher strike **using the long call itself as collateral**, instead of posting fresh underlying. The long option (plus exactly the cash needed to exercise it) is escrowed inside the written position. If assignment ever reaches the compressed range, anyone can permissionlessly exercise the escrowed long to source the underlying — so the written option remains fully backed at all times, while the maker's capital outlay drops from the full underlying to the spread's worst-case cost.

The same mechanism exists for puts. Together, offset closure and spread compression let an options maker run a book that is *fully collateralized in every state* while committing far less capital than naive 1:1 backing would require.

## What this adds up to

| Cost in a traditional on-chain setup | On Pismo |
|---|---|
| Gas per quote update | Zero — quotes are signed messages |
| Collateral locked per outstanding quote | Zero — one pot backs all quotes, checked at fill |
| Separate capital per venue | One vault pool backs options and exchange simultaneously |
| Written-option collateral locked until expiry | Freed early via offset closure |
| Full collateral per written option | Reduced to spread risk via compression |

None of these savings come from leverage or undercollateralization. Every option remains fully backed, every fill remains atomic, and every shortfall reverts rather than socializing a loss. The efficiency comes from eliminating *dead* capital — collateral that secures nothing — not from thinning the guarantees.
