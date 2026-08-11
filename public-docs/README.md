# Pismo Protocol

Pismo Protocol brings **American-style covered-call options** fully on-chain on the Sui blockchain — plus automated covered-call vaults built on top of them.

## What makes it different

Most on-chain options protocols track every writer's position individually, making exercise assignment expensive and unfair. Pismo Protocol uses a **pooled-bucket model with FIFO exercise assignment**:

- All writers of the same contract (asset, expiry, strike) share a single on-chain **bucket**.
- Exercises are assigned to writers in the order they wrote, via a single cursor that advances in O(1) — no per-position bookkeeping.
- Each option is an ordinary fungible **`Coin`** on Sui, so your wallet displays it as a balance and you can split, combine, or transfer it like any other coin.

This design has a clean economic property: early writers received lower premiums (the option was less valuable when they wrote) and face exercise first; late writers received higher premiums and sit deeper in the queue. **Exercise risk always corresponds to the premium you were paid.**

## The products

| Product | What it does |
|---------|--------------|
| **Options protocol** | Write, buy, exercise, and redeem covered calls. Prices come from competing market makers over a request-for-quote (RFQ) system; settlement is fully on-chain. |
| **Covered-call vaults** | Deposit an asset once; the vault automatically sells ~0.10-delta weekly calls through an on-chain auction and rolls the position every week. |

## Where to start

- **New to options on Pismo Protocol?** Read [Options & Buckets](concepts/options-and-buckets.md) to understand the core model.
- **Want to trade?** Follow the [Getting Started](guides/getting-started.md) guide.
- **Want passive yield?** See [Covered-Call Vaults](concepts/vaults.md).
- **Security-minded?** Read the [Security & Trust Model](security.md).
