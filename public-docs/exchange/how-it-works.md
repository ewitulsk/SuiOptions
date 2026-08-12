---
description: "Signed orders, escrow accounts, off-chain matching, and the two on-chain settlement paths of Pismo Exchange."
---

# How It Works

## Signed orders

An order on Pismo Exchange is a message the maker signs with an ordinary Sui wallet. It fixes everything economic:

* the **market** (trading pair) it belongs to,
* the amounts on both sides — which together define the price,
* a **fee ceiling** the maker is willing to pay (*fee details: information coming soon*),
* an **expiry** and a monotonically increasing **salt** (the replay- and bulk-cancel counter),
* the **escrow account** the maker's side will settle from,
* optional restrictions on who may take or submit the fill.

The signature covers all of it, *plus the specific market's on-chain identity* — so an order can never be replayed on a different pair, a different deployment, or a different network. Nothing about a signed order can be altered by anyone, including the exchange itself.

Placing an order is a free HTTP request. So is repricing (sign a replacement) and cancelling. The chain is only involved when a fill happens.

## Escrow: the balance manager

Every maker holds a personal on-chain **escrow account**. It's the Sui-native replacement for the token-allowance pattern on other chains:

* Funds are deposited by the owner and sit visibly on-chain.
* **Only the settlement contract can debit it, and only against the owner's signed order.** There is no other spend path — not for the exchange service, not for the protocol admin.
* **Withdrawal is owner-only and instant**, and deliberately independent of the exchange's pause state. Exit never requires anyone's cooperation.
* The owner can authorize a small set of **delegated signing keys** — hot keys for trading bots. Revoking a key instantly voids every outstanding order it signed, which doubles as a free, immediate "cancel everything from that key" switch for compromised-key response.

Escrow is a shared pot, not per-order locking: outstanding orders are claims against the same balance, and a fill that exceeds it simply reverts. This is the same capital-efficiency choice made throughout Pismo — see [Capital Efficiency](../capital-efficiency.md#no-locked-collateral-per-quote).

## Matching

The off-chain engine keeps a standard price-time priority book per market: best price first, first-come-first-served within a price level. It is written as a deterministic state machine — same input sequence, same output, replayable for audit. Self-trades are prevented by cancelling the newer order.

When two orders cross, the execution price is the **resting (earlier) order's price**, and any difference from the newer order's limit is passed to the newer order as price improvement — the standard fairness rule of limit-order markets. Both signed limits are re-verified on-chain regardless; matching can *propose* a fill but can never move either side past the price it signed.

## Settlement: two paths to the same chain

**Path 1 — matched settlement.** When the engine crosses two resting orders, the exchange's relayer submits an on-chain transaction carrying *both* signed orders. The settlement contract re-validates each signature, expiry, salt, and fill state, moves funds between the two escrow accounts, takes fees from each side's proceeds, and records the fill. Neither trader pays gas — relayer costs are covered from exchange fees.

**Path 2 — open orderbook.** Every resting signed order is also public. Anyone — a swap router, an arbitrageur, another trader — can take an order straight to the chain themselves: pass in a coin, name the order, receive the maker's tokens and change back, in one composable call. No account, no deposit, no permission, no relayer. This is the path swap routers use, and it's what makes the exchange behave like an AMM from a router's point of view — see [vs. On-Chain Order Books](vs-onchain-orderbooks.md).

Both paths converge on the same on-chain fill accounting, keyed by the order's unique digest — so an order can never be filled beyond its signed size no matter how many parties race to take it, and the on-chain record is the single source of truth the exchange's own book reconciles against.

## Cancellation: three tiers

1. **Soft cancel** — a free, signed request to the exchange to stop matching an order. Instant, but honest in its limits: if the order allowed anyone to submit fills, a copy someone already downloaded remains technically fillable on-chain until it expires. The exchange tells you this explicitly when it applies.
2. **Hard cancel** — a small on-chain transaction that marks one order permanently unfillable, no matter who holds a copy.
3. **Salt watermark** — one on-chain transaction that voids **all** of your orders below a chosen salt in a market. Because bots sign orders with increasing salts, this is a cheap "cancel everything older than now" dead-man switch — one transaction, entire book gone, across every market if desired.

The layering is deliberate: day-to-day trading uses free soft cancels and short expiries; the on-chain tiers exist so that certainty is always purchasable, and cheap.

## Vault makers: direct escrow

A [Pismo Vault](../vaults/overview.md) can make markets on the exchange without moving capital into a separate escrow account. In **direct-escrow mode**, the vault's own free balances back its signed orders: a fill settles straight out of — and into — the vault, inside a settlement flow that does all the normal signature and fill-state verification first, and aborts entirely if the vault's balance or its custody rules can't support the fill. Router takers interact with vault orders through the same open-orderbook path as any other order.

This is the mechanism that lets one pool of vault capital quote the options protocol and the exchange simultaneously — the centerpiece of the [capital-efficiency story](../capital-efficiency.md#one-capital-pool-two-venues).
