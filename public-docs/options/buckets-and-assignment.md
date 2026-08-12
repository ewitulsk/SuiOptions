---
description: "The pooled-bucket model behind Pismo Options — one shared pool per contract, FIFO exercise assignment in constant time, and puts that can never be insolvent."
---

# Buckets & Assignment

## The bucket

A **bucket** is a single shared object on Sui identified by the tuple *(underlying asset, expiry, strike, settlement asset)* — for example, "SUI calls at a $4.00 strike expiring Friday, settled in USDC". Every writer of that exact contract deposits into the same bucket, and every option of that contract is drawn from it.

A call bucket holds:

* the **underlying** deposited by writers — the collateral backing every call,
* the **settlement asset** paid in by exercisers,
* two monotonic counters — the *write cursor* and the *exercise cursor* — that drive assignment.

A put bucket mirrors this with cash collateral: writers deposit the full strike value in the settlement asset, and exercisers deliver the underlying.

New buckets are listed by the protocol on a rolling schedule — fresh expiries with a strike grid sized to current volatility — so there is always a live option chain without anyone needing to request a listing.

## The option is a coin

Each bucket's option is a real fungible coin with its own unique currency type, and the bucket holds that currency's **sole mint authority**, never exposing it to anyone. Three guarantees follow:

* **Supply is always honest.** Options are minted 1:1 when collateral is written in, and burned on exercise or after expiry. Outstanding coin supply always equals outstanding option amount.
* **Bucket isolation is a type guarantee.** An option coin can only ever be burned by the one bucket whose treasury minted it. There is no ID field to spoof and no runtime cross-bucket check to get wrong — the Sui type system enforces it.
* **Wallets just work.** The option appears as a normal coin balance. Split, combine, and transfer it like any other coin — or sell it on [Pismo Exchange](../exchange/overview.md).

## The FIFO cursor

Every write occupies a contiguous range on an unbounded number line: your range starts where the previous writer's ended, and the write cursor advances by your amount. Your **Position** — a transferable on-chain object — records exactly that range.

When a holder exercises `N` options, the bucket simply advances the exercise cursor by `N`. No individual position is touched; the cost is constant regardless of how many writers share the bucket.

After expiry, your outcome is determined entirely by where your range sits relative to the final cursor:

* **Entirely behind the cursor** → you were fully exercised. For a call: you receive `amount × strike` in the settlement asset (and you already kept the premium). For a put: you receive the underlying that was delivered to you.
* **Entirely ahead of the cursor** → you were never exercised: your full collateral comes back.
* **Straddling the cursor** → partially exercised, pro-rated exactly at the cursor position.

This is mathematically identical to first-in-first-out assignment, computed in constant time.

### Why FIFO is fair

Writers who came early wrote when the option was cheaper and received a smaller premium — they face exercise first. Writers who came late received a richer premium and are only exercised under heavy exercise pressure. **Your exposure to assignment always tracks the premium you were paid.** And because assignment is a deterministic function of two on-chain counters, there is no randomness and no operator discretion anywhere in the process.

### Why this beats per-position bookkeeping

Protocols that track each writer individually must touch writer state on every exercise — which makes exercise cost scale with the number of writers, makes partial exercise awkward, and usually forces either pro-rata assignment (which punishes early writers) or an off-chain assignment process (which requires trusting whoever runs it). The bucket model gets deterministic, on-chain, constant-time assignment with an economically sensible ordering, at the cost of one design constraint: all writers of a contract share one pool.

## Puts: solvent by rounding

Put buckets add one subtlety worth spelling out, because it's a guarantee, not an implementation detail. All strike math on the put side rounds **against the pool's obligations**:

* Collateral demanded from writers is rounded **up** — the pool always holds at least what it may owe.
* Every cash payout is rounded **down**.

The result is that a put bucket is *unconditionally solvent* — there is no sequence of writes, exercises, and redemptions that can leave it unable to pay — at the cost of bounded dust that is swept when the bucket is cleaned up after expiry. Call buckets need no such treatment because their collateral (the underlying itself) is never subject to rounding.

## Netting and compression

Two additional mechanics operate on positions inside a bucket:

* **Offset closure** — a writer who also holds the same bucket's option coins can net them against their written range and withdraw the freed collateral immediately, before expiry. The exercise cursor permanently skips the closed range.
* **Spread compression** — a written option can be collateralized by an escrowed long option (at an equal-or-better strike, equal-or-later expiry) plus exactly the cash to exercise it, instead of by fresh collateral. If assignment ever reaches a compressed range, anyone can permissionlessly trigger the escrowed long's exercise so the pool receives real underlying — full backing is preserved in every state.

Both exist for capital efficiency and are covered in depth on the [Capital Efficiency](../capital-efficiency.md) page.

## Lifecycle of a bucket

1. **Listed** — created by the protocol scheduler with its expiry, strike, and settlement asset fixed forever.
2. **Live** — writes, trades, exercises, offsets, and compressions flow freely until expiry.
3. **Expired** — no new writes or exercises. Holders' unexercised coins are worthless; writers redeem their Positions for their FIFO outcome.
4. **Cleaned up** — once drained, the bucket is removed and residual rounding dust is swept.

The protocol can pause *new writes* into a specific bucket (for example, if a listing was misconfigured), but pausing never blocks exercises or redemptions — money already in a bucket can always come out through the normal rules.
