---
description: "The risks of depositing into a Pismo Vault, in plain language — read this before you deposit."
---

# For Depositors

This page is the plain-language version of everything a depositor should understand before putting money into a vault. It repeats things said elsewhere, on purpose.

## What you're actually buying

Depositing into a vault buys you a proportional share of an actively traded portfolio, managed by a curator whose custody is constrained by contracts but whose *trading judgment is not*. Your return is the curator's trading performance, minus their performance fee. This is closer to allocating to a trading desk than to holding an index — with the crucial difference that the desk cannot run off with the money.

## What protects you

* **Theft is structurally prevented.** No vault function pays the curator or any third party from vault funds. Every trading path returns proceeds to the vault; the one external channel is budget-capped, destination-locked, and jointly key-controlled. Read [The Curator Security Model](curator-security-model.md) — it's written to be checkable, not reassuring.
* **Entry and exit prices are honest.** Every deposit and withdrawal is priced against a complete, fresh, attested valuation. Conservative marking and anti-manipulation share math mean the share price can't be gamed against you at entry or exit.
* **Exit never needs the curator.** Withdrawals queue and are fulfilled permissionlessly. If the queue starves past the vault's grace period, anyone can force-unwind the vault's on-chain positions to free your money. A closed or abandoned vault can be fully drained by its depositors without any privileged party.
* **Fees are on your profit only.** The performance fee crystallizes at your withdrawal, on your individual realized gain. No profit, no fee. (*Fee rates: information coming soon.*)

## What does NOT protect you

* **Nothing prevents trading losses.** The curator quotes and trades at whatever prices they judge right. Bad judgment, bad markets, or malicious self-dealing (trading deliberately badly against an accomplice) all show up as losses in the share price. The contracts make losses *visible quickly* and *bounded in speed* through the external channel — they cannot make them impossible.
* **External venue capital carries venue risk.** Capital deployed to an external venue (within its budget cap) is exposed to that venue's own solvency and operation, on top of the curator's trading.
* **Withdrawals are not instant.** Your capital is working. Expect queue time — normally short, but in stress, bounded by the grace period plus the time to unwind positions, and an aged request may be paid in the vault's accounting asset rather than the asset you requested.
* **Share prices move between request and payment.** Queued shares keep earning (and losing) until fulfillment. The price you exit at is the price *then*, not the price when you clicked withdraw.

## Trusted curators vs. public vaults

Vault creation is open to anyone. The Pismo app distinguishes:

* **Trusted curators** — operators vetted by Pismo, including the Pismo-run flagship vault. Vetting covers operational competence and identity; it is a judgment, not a guarantee.
* **Public vaults** — created by anyone. Identical on-chain protections, zero vetting of the human.

The honest framing: the contracts equalize *custody* risk between the two, but they cannot equalize *competence* risk. A public vault run by an anonymous curator has exactly as strong a guarantee that funds can't be stolen — and none of the vetting on whether they'll be traded well. Size your deposits accordingly.

## A note on the flagship vault

The initial flagship vault is curated by Pismo itself, market-making [Pismo Options](../options/overview.md) to bootstrap protocol liquidity. That means early depositors are funding — and earning from — the market-making of a protocol operated by the same team, which is a concentration of roles we think is worth naming out loud. It is intended as a bootstrap arrangement: we are working to hand the curator seat to an independent, professional market-making firm, and the capability-based curator design exists precisely so that handover changes nothing about depositor protections.

## Quick self-check before depositing

1. Do I understand that I can lose money from the curator's trading, and no contract prevents it?
2. Do I know who the curator is, and whether they're a trusted curator or a public vault?
3. Am I comfortable with queued (not instant) withdrawals, including the stress-case timeline?
4. Do I understand which venues this vault has enabled — in particular whether it deploys capital to external venues?
5. Have I read [Limitations & Trust](limitations-and-trust.md)?

If any answer is no, read further or deposit less.
