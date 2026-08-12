---
description: "Deposits, ledger shares, attested valuations, and the FIFO withdrawal queue — the accounting machinery of a Pismo Vault."
---

# How Vaults Work

This page covers the depositor-facing machinery: how money goes in, how it's valued while it's inside, and how it comes out.

## Deposits

A vault denominates its accounting in one **accounting asset** (say, USDC), but the curator can enable a small list of additional accepted deposit assets. Depositing works like this:

1. Your deposit is valued against a **complete, fresh valuation** of the vault (see below) — never against a stale or partial number.
2. Non-accounting-asset deposits are converted at an attested oracle price, minus a small curator-configured **entry haircut** (bounded by the contract) that protects existing depositors from price staleness around your entry.
3. You're minted **shares** proportional to the value you contributed relative to the vault's net asset value.

Shares are **ledger entries, not tokens** — they cannot be transferred or sold, which keeps every depositor's cost basis honest and makes the performance fee chargeable on your actual profit rather than some average. The share math includes a standard virtual-offset defense that makes first-depositor share-price manipulation unprofitable, and the vault has **no donation path** — nobody can distort the share price by sending assets in from outside the deposit flow.

The curator can pause deposits at any time (existing funds are unaffected), and curators are typically required to keep skin in the game: a minimum share of their own vault that they cannot fully withdraw while the vault is open.

## Valuation: the appraisal

Every deposit and every withdrawal batch consumes an **appraisal** — a same-transaction proof that *everything* the vault holds was just valued:

* free balances in every held asset, priced by allowlisted oracle attestations,
* every custodied venue position — resting DeepBook orders, exchange escrow, written option positions, held option coins — each valued by its own venue integration under conservative marking rules (option positions are marked at intrinsic value plus a bounded, decaying time-value estimate — never at an optimistic model price),
* any capital deployed to an external venue, via a guardrailed equity attestation (see [Venues](venues.md#external-venues)).

If any component is missing, or if anything in the vault moved between the start of the valuation and its use, the transaction aborts. There is no code path that prices your entry or exit against yesterday's number — the freshness check is structural, not a convention.

Between user actions, the protocol's keeper service periodically runs the same valuation in a read-only mode, purely so the app can display up-to-date share prices. Anyone can run this crank; it can't move funds. Rounding throughout the share math consistently favors *remaining* depositors — dust accumulates to the vault, never leaks from it.

## Withdrawals: the FIFO queue

Withdrawals are queued rather than instant, because vault capital is usually *working* — resting in orders, backing option quotes, deployed to venues. The flow:

1. **Request.** You ask to redeem some or all of your shares, naming which asset you want to be paid in (any asset the vault holds, not just the accounting asset). Your request joins a first-in-first-out queue.
2. **Earn until paid.** Queued shares keep their exposure — profit and loss continue to accrue to you until the moment your request is fulfilled, and your performance fee is crystallized at that same moment, on your actual realized profit.
3. **Fulfillment.** The keeper (or anyone — the crank is permissionless) fulfills the queue against a fresh appraisal. A whole batch is paid at one consistent share price. A small curator-configured **exit haircut** (contract-bounded) applies on conversion into non-accounting payout assets.
4. **Payment is all-or-nothing per request** — you receive exactly your share value in your chosen asset, or the fulfillment waits until the vault has freed enough of it.

### The liveness backstop

What if a curator leaves everything deployed and the queue starves? Each vault has an on-chain **grace period**. Once the oldest withdrawal request has waited longer than the grace period, two permissionless escape hatches unlock:

* **Force-unwind**: anyone can cancel the vault's resting venue orders and sweep freed funds back to the vault — through special sessions that can only *return* money to the vault, never take anything out.
* **Fallback payment**: an aged request may be paid in the accounting asset regardless of the asset it originally requested, so a request can't be held hostage by one illiquid asset.

Getting your money out never depends on the curator's cooperation — only on the vault's positions being unwindable, which the force-session design guarantees for every on-chain venue.

## Fees

The fee model is Morpho-style: the **curator** charges a performance fee on each depositor's individual realized profit, crystallized at withdrawal; the **protocol** takes a percentage *of the curator's fee* — never a separate charge on depositors. The curator's fee is paid in shares (keeping the share price unchanged for everyone else), and both rates are capped by the contract. Published numbers: *information coming soon*.

## Vault lifecycle

A vault moves through three states: **Open** (normal operation), **Closing** (initiated by the curator or protocol; no new deployment of capital, positions being unwound — force-unwind rights unlock for everyone immediately), and **Closed** (nothing remains but the accounting asset; every remaining depositor is queued and paid out — permissionlessly, so nobody can be stranded by an absent curator). A vault can only reach Closed once every position is unwound and all external capital has been returned.
