---
description: "Where vault capital can be deployed — Pismo Options, Pismo Exchange, DeepBook, and external hedge venues like Bluefin — and what keeps each integration safe."
---

# Venues

A vault's capital works by being deployed. Each venue is reached through its own integration — a separate, allowlisted contract that adapts the venue's mechanics to the vault's custody rules. The vault itself stays venue-agnostic: it knows integrations only by their registered identity, and enforces the same session discipline on all of them.

For a vault operating as an options market maker, the venues play distinct roles: **Pismo Options** is where the vault earns — quoting two-sided markets in premium. The others are where it manages the risk that quoting accumulates: offloading option inventory on **Pismo Exchange**, adjusting spot exposure on **DeepBook**, and — in the market-making context — using **external venues** as *hedge venues* for exposure that spot alone can't neutralize.

## Pismo Options: the vault as market maker

The vault implements the options protocol's [collateral-release interface](../options/market-making.md#the-collateral-interface-bring-your-own-funding), so the curator's trading system can sign options quotes funded directly by vault capital. When a trader executes such a quote, the vault releases exactly the quoted funds — under conditions stricter than any standalone maker account:

* The signed quote must name **the vault itself as recipient** of everything the maker side receives — premiums, option coins, returned collateral. A quote routing proceeds anywhere else is structurally unexecutable. This single check is what makes it safe for depositor funds to back a curator's signature.
* The curator holds a per-vault **kill switch**: vault-funded quoting can be disabled instantly, independent of anything else.
* Option positions and coins the vault acquires are custodied as tagged vault positions. Exercising, redeeming after expiry, and [offset-closing](../capital-efficiency.md#netting-options-positions-early) run through the integration's controlled paths; conservative marking rules value them in every appraisal.

Anything a fill sends to the vault's address is swept into vault custody by permissionless cranks — value in flight to the vault never depends on the curator to arrive.

## Pismo Exchange: quoting from the vault balance sheet

The exchange integration supports two custody modes, both keeping the venue's escrow account under an ownership design that no person controls — its withdrawal rights live inside the vault as a custodied position:

* **Funded mode** — working capital is moved into the exchange escrow account through curator sessions; the account's on-chain balance is read directly for valuation.
* **Direct mode** — the escrow account is identity-only, and the vault's own free balances back the vault's signed orders. Fills settle straight out of, and into, the vault inside the settlement flow, which pre-checks the vault's balance and aborts cleanly if it can't cover the fill. This is the mode behind the [one-pool-two-venues](../capital-efficiency.md#one-capital-pool-two-venues) capital-efficiency story.

In both modes the vault's orders are ordinary signed exchange orders — takers and swap routers interact with them like any other liquidity. Self-crossing (the vault filling its own order) is blocked on-chain, and force sessions can strip the account's funds and trading keys back to the vault at any time.

## DeepBook: on-chain spot

For spot execution the vault trades DeepBook, Sui's canonical on-chain order book. The integration's custody trick: the vault's DeepBook trading account is created and then **permanently wrapped inside a vault position, along with all of its capability objects** — including the withdrawal capability. The venue's own owner-level functions become unreachable; the only paths that remain are the integration's session-gated entry points, every one of which settles proceeds back into the vault.

The curator can place, modify, and cancel orders and execute swaps freely — there are deliberately no price guardrails on curator trading (see [the security model](curator-security-model.md#what-this-model-does-not-prevent)) — but a protocol-level **pool allowlist** bounds *which markets* vault capital can rest in. Settled balances are swept back by permissionless cranks, and force-unwind can cancel everything and bring all funds home without the curator.

## External venues

Some venues cannot be custodied on-chain at all. The first integration is **Bluefin**, a Sui derivatives exchange, whose accounts are controlled by signature authority rather than on-chain objects — there is nothing for the vault to hold. (An integration with **Aftermath Finance** is next on the roadmap.)

For these, the vault uses the [external-account channel](curator-security-model.md#the-one-exception-external-accounts): a single registered address, a budget capped as a percentage of vault value, a rolling daily release limit, and returns accepted only from the account itself. On top of those vault-side bounds, the Bluefin integration adds joint key custody:

* **The account's key is split.** The venue account is controlled by a 2-of-2 threshold signature: one share held by the curator, one by Pismo's signing service. Neither party can sign alone, and the full key never exists anywhere — it's generated by a distributed ceremony between the curator's browser and the service.
* **The protocol half only signs safe shapes.** The signing service applies a strict, fail-closed policy: it will co-sign venue logins, authorization of the curator's designated trading key, withdrawals (which, by the venue's own design, can only pay back to the joint account), deposits that credit only the joint account, and sweeps that return funds only to the vault. Anything else — any unrecognized payload, any transfer to a third address — is refused and logged.
* **Day-to-day trading needs no ceremony.** Once the curator's trading key is authorized on the venue, ordinary trading happens at full speed with the curator's own key. The joint key is only needed at the boundaries — moving money in, moving money out — which is exactly where the protection belongs.
* **Equity is continuously attested.** While capital is deployed, the protocol's keeper polls the venue account's value and posts it on-chain through a guardrailed oracle (rate-limited, delta-limited, staleness-checked — see [Oracles & Attestations](../infrastructure/oracles.md)). The vault requires this attestation fresh for every deposit and withdrawal, and a reconciliation monitor alerts if deployed capital and attested equity diverge.

The residual risk is stated plainly in [the security model](curator-security-model.md#what-this-model-does-not-prevent) and in [Limitations & Trust](limitations-and-trust.md): within its budget, external capital is exposed to the curator's trading decisions and to the venue itself. The bounds control the blast radius; they don't eliminate it.

{% hint style="warning" %}
The Bluefin integration is early-stage and currently exercised only in staging environments. It will be hardened and validated before mainnet vaults can enable it.
{% endhint %}
