# Options & Buckets

## The bucket

A **bucket** is a single shared object on Sui identified by the tuple *(underlying asset, expiry, strike, settlement asset)* — for example, "SUI calls at a $4.00 strike expiring Friday, settled in USDC". Every writer of that exact contract deposits into the same bucket, and every option of that contract is drawn from it.

A bucket holds:

- the **underlying** deposited by writers (the collateral backing every call),
- the **settlement asset** paid in by exercisers,
- two monotonic counters — `total_written` and `exercise_cursor` — that drive FIFO assignment.

## The option is a coin

Each bucket's option is a real fungible `Coin` with its own unique currency type. The bucket holds the sole `TreasuryCap` for that currency, which means:

- **Supply is always honest.** Options are minted 1:1 when underlying is written in, and burned on exercise or after expiry. The coin's outstanding supply always equals the outstanding option amount.
- **Bucket isolation is a type guarantee.** An option coin can only ever be burned by the one bucket whose treasury minted it. There is no ID field to spoof and no runtime cross-bucket check to get wrong — the Sui type system enforces it.
- **Wallets just work.** The option appears as a normal coin balance. Use ordinary coin split/join to subdivide or recombine positions, and transfer them freely.

## The FIFO cursor

Every write occupies a contiguous range `[start, end)` on an unbounded number line: your range starts where the previous writer's ended, and `total_written` advances by your amount.

When a holder exercises `N` options, the bucket simply advances `exercise_cursor` by `N`. No individual position is touched.

After expiry, your outcome is determined entirely by where your range sits relative to the final cursor:

- **Entirely behind the cursor** → you were fully exercised: you receive `amount × strike` in settlement asset.
- **Entirely ahead of the cursor** → you were never exercised: you get all your underlying back (and you already kept the premium).
- **Straddling the cursor** → partially exercised, pro-rated exactly at the cursor position.

This is mathematically identical to first-in-first-out assignment, but each exercise costs O(1) regardless of how many writers share the bucket.

## Why FIFO is fair

Writers who came early wrote when the option was cheaper (less in-the-money) and received a smaller premium — they face exercise first. Writers who came late received a richer premium and only get exercised if the bucket faces heavy exercise pressure. **Your exposure to exercise always tracks the premium you were paid.** And because assignment is a deterministic function of on-chain counters, there is no assignment lottery and nothing for an operator to manipulate.
