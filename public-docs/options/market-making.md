---
description: "How market makers participate in Pismo Options — a signed identity, free off-chain quoting, and a pluggable collateral interface."
---

# Market Making

Market makers are the pricing engine of Pismo Options. This page describes how participation works conceptually — what a maker's on-chain footprint is, how quoting works, and where the capital behind quotes lives.

{% hint style="info" %}
This is a conceptual description, not an integration guide. Developer documentation for connecting a market-making system is *coming soon*.
{% endhint %}

## A maker's on-chain footprint is tiny

Becoming a market maker takes two on-chain steps, done once:

1. **Register a signing identity.** A maker creates a **quote signer** — a small shared object holding their public key and chosen signature scheme. This object's identity is embedded in every quote the maker signs, and its key can be rotated by the maker at any time. The object also stores the used-nonce set that makes quote replay impossible.
2. **Establish a funding source.** The capital that backs quotes lives in a collateral account the maker controls (or a vault — see below).

After that, everything is off-chain. The maker holds a WebSocket connection to the quoting service, receives quote requests, prices them, and returns signed quotes. **No transaction, no gas, no per-quote cost of any kind.** A quote cancels itself by expiring; there is nothing on-chain to clean up. This is the free-quoting model described in [Capital Efficiency](../capital-efficiency.md).

Makers can also opt into serving **indicative prices** — lightweight, unsigned premium estimates that power the option-chain display without consuming a nonce or committing the maker to anything.

## The collateral interface: bring your own funding

The most architecturally interesting part of Pismo Options is what happens at fill time. The protocol core doesn't know or care where a maker's money lives. Instead:

* Every signed quote names its **funding source** — which account, and which code, will release the maker's side of the trade.
* At fill time, the chain verifies the quote's signature, then asks that exact source to release exactly the required amount, **inside the same transaction**.
* The design guarantees a funding source can only ever *refuse* (aborting the whole trade) — it can never redirect funds, short-change the fill, or execute at different terms, because everything economic was fixed inside the maker's signature.

Two funding implementations exist today:

| Funding source | Who it's for | Key property |
|---|---|---|
| **Personal collateral account** | Independent makers | The maker publishes their own small collateral contract; deposits are open, withdrawals are owner-only. The account is the maker's identity boundary. |
| **A Pismo Vault** | Curators market-making with pooled capital | The vault releases funds for quotes signed by its curator's bot — but *only* if every coin the maker side receives routes back to the vault, and only while the vault's per-vault kill switch is enabled. Depositor funds can back quotes without the curator ever being able to skim proceeds. See [Pismo Vaults](../vaults/venues.md). |

Because the interface is standardized and permissionless, other custody designs — a multisig treasury, a credit facility — can implement it without asking anyone's permission.

## What the quoting service does (and doesn't do)

The quoting service is a router. It authenticates makers against their on-chain registered keys (a challenge-response signature check), broadcasts quote requests, validates responses, and returns them to traders sorted best-first.

Deliberate design choices worth knowing:

* **Requests carry only the bucket's address** — never the strike, expiry, or asset types. Makers resolve contract details independently, so even a compromised quoting service cannot trick a maker into pricing the wrong contract.
* **The service holds no funds, holds no keys, and performs no balance checks.** Whether a maker can actually cover a quote is enforced by the atomic on-chain fill — a shortfall reverts the whole transaction. This is deliberate: a funding source (like a vault or credit line) need not expose a readable balance at all, so feasibility genuinely cannot be checked off-chain.
* **Reputation is tracked, not gated — yet.** The service counts each maker's signed, executed, expired, and reverted quotes, and surfaces a composite reliability score used to sort quotes. Today a low score deprioritizes a maker; it does not exclude them. A fuller reputation system — where failed user transactions cost the responsible maker standing, with real consequences — is planned as retail-operated strategies come online.

## Managing inventory

A maker who fills options flow accumulates positions: written Positions, long option coins, directional exposure. The system is built so all of it can be recycled:

* **Option coins are ordinary coins** — quote them for sale on [Pismo Exchange](../exchange/overview.md), including to swap-router flow.
* **Offset closure** nets bought-back options against written positions, freeing collateral mid-cycle.
* **Spread compression** collateralizes new writes with existing longs instead of fresh capital.
* Vault-based makers additionally get spot venues (DeepBook) and external derivatives venues for hedging — see [Venues](../vaults/venues.md).

Both netting primitives are covered in [Capital Efficiency](../capital-efficiency.md#netting-options-positions-early).
