# Security & Trust Model

## What the chain enforces

The critical invariants are enforced by Sui Move, not by any off-chain service:

- **Full collateralization.** Every call is backed 1:1 by underlying locked in its bucket. There is no undercollateralized writing.
- **Honest supply.** Option coins are minted only when collateral enters and burned on exercise/expiry, through a treasury the bucket alone controls. Outstanding supply always equals outstanding options.
- **Deterministic assignment.** Exercise assignment is a pure function of two on-chain counters. No operator chooses who gets assigned.
- **Quote integrity.** Trades execute only against a market maker's Ed25519-signed quote, checked on-chain for signature validity, expiry, and single-use nonce. Quotes cannot be replayed, altered, or forged.
- **Atomicity.** A trade either fully executes — collateral in, premium routed, position and option minted — or fully reverts. There are no partial states.

## What off-chain services can and cannot do

| Service | Holds funds? | Worst case if compromised |
|---------|--------------|---------------------------|
| Quoting service | No | Censor or re-order quotes; cannot move funds |
| Indexer | No | Serve stale data to apps; cannot move funds |
| Keeper (vault crank) | Gas wallet only | Stall a vault round until someone else cranks it — the crank is permissionless |
| Scheduler (bucket creation) | Admin capability | Create bogus buckets or misconfigure fees; cannot touch user funds in buckets, accounts, or vaults |

Market makers are treated as **untrusted**. A maker who signs quotes it can't back just causes a failed transaction — the on-chain balance check is the safety net, and reputation tracking filters persistent offenders out of quote results.

## Oracle guardrails

Vault proceeds conversion runs through an on-chain auction whose acceptable price range is bounded by an external price oracle, preventing settlement-to-underlying conversion at manipulated prices.

## Admin powers

The admin capability can create buckets, set protocol fees, and withdraw accumulated **protocol fees** — it cannot withdraw user funds from buckets, accounts, or vaults. Vault round progress does not depend on any admin key: the entire lifecycle is permissionlessly crankable, and kill switches are designed to never strand user funds.
