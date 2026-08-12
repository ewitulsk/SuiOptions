---
description: "How Pismo Exchange's hybrid model compares to fully on-chain order books and off-chain RFQ systems — and why it composes with swap routers."
---

# vs. On-Chain Order Books

Sui already has excellent fully on-chain order books — DeepBook is the canonical one, and Pismo itself uses DeepBook as a spot venue for its vaults. Pismo Exchange isn't an attempt to replace that model; it's a different point in the design space, chosen for a specific participant: the active market maker. This page lays out the trade honestly.

## The cost-model inversion

| | Fully on-chain book | Pismo Exchange (hybrid) |
|---|---|---|
| Order lives | on-chain, as an object | off-chain, as a signed message |
| Place / reprice / cancel | a transaction each, costs gas | an HTTP request, free |
| Quote update rate | bounded by chain throughput and gas budget | bounded by how fast you can sign |
| Fill | on-chain | on-chain (always) |
| Matching trust | none — the chain orders trades | the service, for liveness and fairness of matching only |
| Taker prerequisites | typically an account/balance object | none — a coin in a transaction |
| Custody | pool or account object | personal escrow, owner-only instant withdrawal |

The essential trade: a fully on-chain book needs zero trust in any operator for *matching*, at the cost of making every quote update a paid transaction. The hybrid model makes quoting free, at the cost of trusting an operator for **liveness and match ordering — and provably nothing more**, since every fill still requires the maker's on-chain-verified signature and both signed price limits are re-enforced at settlement.

For a maker whose fair price moves every second — especially one quoting **option tokens**, whose value ticks with the underlying — that trade is decisive. On a gas-per-update book, the rational response is wide spreads and slow updates. When updates are free, the rational response is tight spreads and constant repricing. Takers capture that difference.

## The RFQ comparison

Off-chain RFQ systems (common in professional OTC-style flows) also offer free quoting — but the quote only exists in response to a specific request, is typically bound to one taker, and lives outside the public, composable liquidity that routers aggregate. Pismo Exchange keeps the free-quoting property while making every resting order **public and open-access**: any party can pull a signed order and settle it on-chain without asking. It behaves like an order book to traders, like an RFQ system to makers, and like an AMM to routers.

(Pismo Options runs a true RFQ flow — request, quote, fill — because options trades are sized, directional, and quote-driven by nature. The exchange serves the resting-liquidity side of the market. The two share the same signature-verified, atomic-settlement architecture.)

## What "router-compatible" actually means

Swap routers and aggregators want liquidity sources that look like functions: coin in → coin out, composable in one atomic transaction, no account setup, no side effects if anything fails. AMMs have this shape natively. Order books usually don't — filling typically requires an account, a deposit, or an order object interaction.

Pismo Exchange's taker-side fill is exactly the function shape routers want:

* **Coin in, coins out.** Pass a coin and an order reference; receive the maker's tokens and your change. No account, no deposit, no registration.
* **Composable.** A route can chain a Pismo fill after an AMM hop and before another, split across several Pismo orders in parallel branches, and mix venues freely — all in one transaction.
* **Atomically slippage-protected.** A multi-branch route sets one minimum-output check at the very end; if the joined result falls short, the *entire* route reverts. Intermediate legs never need their own guards.
* **Stale-safe.** If someone else consumed an order first, the fill aborts — it can never execute at worse terms than the order was signed for. A router integration can be aggressive about using cached quotes because the failure mode is a revert, not a bad fill.

The exchange also serves a route-planning endpoint that returns split routes across its own books along with everything needed to build the transaction — but third-party routers are equally free to plan their own routes from the public book, since the orders themselves are the fill tickets.

## Complementary, not rival

Inside the Pismo ecosystem the two book models coexist by design: [Pismo Vaults](../vaults/venues.md) trade DeepBook for spot execution *and* quote Pismo Exchange as makers, from the same capital. Deep, chain-ordered venues and free-quoting hybrid venues are good at different jobs; the vault architecture treats both as pluggable venues rather than picking a winner.
