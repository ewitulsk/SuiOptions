---
description: "What Pismo Exchange's settlement contract enforces, what the matching service is trusted for, and the honest caveats."
---

# Limitations & Trust

## What the chain enforces

No fill can happen — through the matching engine, through a router, through anyone — unless the settlement contract verifies, on-chain, that:

* the **maker's signature** covers the exact order being filled, bound to this specific market on this specific deployment;
* the order is **not expired, not cancelled, not voided** by a salt watermark, and not already filled beyond its signed size (fill accounting is keyed per order and checked before any funds move);
* the fill respects **both signed price limits** — matching can propose, but can never move either side past the price it signed;
* fees do not exceed the **maker's signed fee ceiling** or the contract's hard-coded maximum — fee *cuts* apply immediately, but no fee *increase* can ever exceed what an order individually agreed to;
* escrow debits come **only from the account the signed order named**, and credits go only where the settlement rules direct them.

Escrow accounts themselves have exactly one external spend path: the owner's withdrawal. It is instant and works even when the exchange is paused.

## What the matching service is trusted for

**Liveness and fairness of matching. Nothing else.**

| Failure | Consequence | What's protected |
|---|---|---|
| Service goes offline | No new matching; the app degrades | Resting signed orders remain directly fillable on-chain by anyone holding them; every maker can withdraw escrow instantly; all past fills are final on-chain |
| Service censors or reorders | Worse match quality for affected orders | It cannot fill you at a worse price than you signed, cannot forge a fill, cannot touch escrow |
| Service database is lost | Book state is rebuilt | Orders are persisted before they enter the book, and on-chain fill records are the source of truth the book reconciles against — double-settlement is impossible by construction |
| Relayer key is compromised | Attacker can submit matches | Every match still requires two valid maker signatures and passes full on-chain validation — the relayer key can waste its own gas, not your funds |

## Admin powers

The exchange admin can: list markets, pause a market, set fee parameters within the contract's hard caps, and sweep accumulated **protocol fees**. The admin cannot move user escrow, cannot alter or erase fill records, and cannot block withdrawals — pause stops new fills only.

## Honest limitations

* **Not on mainnet yet.** Testnet today; mainnet in progress.
* **Matching requires our service.** Unlike a fully on-chain book, if Pismo's matching engine is down there is no automatic fallback matcher. The open-orderbook path means fills can still happen — anyone can take a resting order on-chain — but continuous two-sided matching is a service we operate. This is the explicit trade the hybrid model makes.
* **Soft cancels are best-effort.** A free off-chain cancel stops the exchange from matching an order, but if the order was open to any submitter, a previously downloaded copy remains fillable on-chain until it expires. The exchange tells you when this caveat applies, and the on-chain cancel tiers (single-order, or bulk-by-salt) exist precisely to buy certainty cheaply. Short order expiries are the practical mitigation.
* **Over-committed makers cause failed fills.** Escrow is a shared pot, not per-order locking. If a maker's outstanding orders exceed their balance, a fill can revert; the exchange detects this and prunes that maker's uncovered orders. Takers lose at most gas.
* **Liquidity is bootstrapping.** Early books are quoted substantially by the Pismo-curated vault (see [Pismo Vaults](../vaults/overview.md)). We say this openly; deeper independent maker participation is the goal, and free quoting is the incentive.
* **Fees: information coming soon.** Fees are charged only on fills, from proceeds, within contract-enforced ceilings — but published rates await mainnet configuration.
