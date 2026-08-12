---
description: "Why a vault curator can trade depositor capital but can never take it — sessions, allowlisted adapters, and structurally-closed exits."
---

# The Curator Security Model

The claim on the [overview page](overview.md) — *the curator trades, but can never withdraw* — is the entire value proposition of a Pismo Vault. This page explains how the contracts make it true, in enough detail that you don't have to take our word for it.

## The curator is a capability, not an identity

Curatorship is held as an on-chain **curator capability object** — transferable, so a curator can be a person, a bot's wallet, a multisig, or a future DAO. The vault recognizes exactly one current capability; rotating to a new one instantly voids the old one's control. The curator's own deposited stake is tied to the capability but survives rotation as a pure claim — an outgoing curator keeps their money, not their power.

What the capability grants: opening trading sessions, configuring vault parameters within contract bounds (haircuts, accepted deposit assets, venue opt-ins, pausing deposits), and initiating closure. What it does not grant is any path that pays the capability holder — the only way a curator extracts value is the performance fee, computed by the contract at each depositor's withdrawal.

## Sessions: every trade is a round trip

The curator never touches vault balances directly. All trading happens inside a **session** — a special object with a deliberate property: it *cannot be stored, dropped, or passed out of the transaction that created it*. The Move language enforces that whoever opens a session must close it before the transaction ends, and closing requires the session's books to balance.

A trading session works like this:

1. The curator opens a session naming one **allowlisted venue integration**.
2. Within the session, funds can be *taken* from vault balances — but only into that integration's calls, and everything the venue returns is *put* back into the vault before the session closes.
3. The session records what was taken, what was returned, and what positions were added or removed. Venue positions the vault acquires (a resting order's escrow, a written option position) are stored **inside the vault itself**, tagged by the integration that owns them — only that same integration can later act on them.

The consequence: there is no state of the world in which vault funds sit in a curator-controlled account. Money is either in the vault, or inside a venue position that only an allowlisted integration can unwind — *back into the vault*.

## Allowlisted integrations: venues are vetted, exits are not

Which venue integrations a vault may use is governed by a protocol-level **allowlist** (and, per vault, by the curator's own opt-ins). Publishing a new integration does nothing until the protocol allowlists it; delisting one instantly stops new deployment through it.

The critical asymmetry: **exit paths are deliberately not allowlist-gated.** The force-unwind and maintenance sessions — the ones that can only cancel orders and return funds to the vault — are permissionless and remain available even for a delisted integration. Governance can stop a curator from *deploying* through a venue; nothing can stop funds from *coming home*.

Each integration also enforces venue-specific custody rules, covered in [Venues](venues.md) — for example, the DeepBook integration permanently wraps the venue's withdrawal rights so they're only reachable through vault-returning code paths, and the options integration refuses any quote whose proceeds don't route back to the vault.

## The one exception: external accounts

Some venues can't be custodied on-chain at all — a derivatives exchange whose accounts are controlled by signatures rather than objects. For these, and only these, a vault may register a single **external account**: an address capital can be *released* to, breaking the same-transaction round-trip rule.

Because this is the one hole in the wall, it is the most heavily bounded thing in the protocol:

* **The destination is fixed.** Releases can only pay the one registered address — registered either by protocol governance, or by the curator through an attested self-serve path that requires cryptographic proof that the *protocol itself co-holds the account's key* (see [Venues](venues.md#external-venues)), is capped at conservative limits, and can only ever be set once.
* **The amount is budgeted.** Total deployment is capped at a percentage of vault value, with a separate rolling daily release limit — both checked against a complete, same-transaction appraisal, so the cap binds against *true* current value.
* **Returns are one-way.** Only the registered account itself can send capital back, and repatriation reduces recorded exposure — it can never be spoofed by a third party to fake a return.
* **The vault refuses to fly blind.** While external capital is outstanding, every deposit and withdrawal requires a fresh, guardrailed attestation of what the external account is actually worth. No attestation, no flows — the vault would rather pause than misprice shares.

## What this model does *not* prevent

Honesty requires stating the residual risk plainly: **a curator can lose money.** Nothing constrains the prices at which a curator trades on integrated venues — no bands, no sanity checks. That is a design decision ("oracle-free trading"): market making requires quoting freely, and any on-chain price constraint tight enough to stop bad trades would also stop good ones.

A malicious curator's realistic attack is therefore not theft but **value destruction** — deliberately bad trades against an accomplice on the other side, or reckless losses at an external venue (within its budget caps). The protocol's answer is layered rather than absolute: budget and rate limits bound how fast value can leave through the external channel; continuous reconciliation monitoring compares deployed capital against attested venue equity and alerts on divergence; conservative marking means losses show up in the share price quickly rather than being hidden; and the trusted-curator designation in the app reflects operational vetting of exactly this risk. For the full depositor-facing risk picture, see [For Depositors](for-depositors.md).
