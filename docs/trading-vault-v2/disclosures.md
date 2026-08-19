# Trading Vault v2 — Issuer and Holder Terms & Risk Disclosures

Terms version 1 (see [spec.md](spec.md)). Vault creation, deposit, purchase
UI, and reset alerts must link to THIS version, never a floating "latest".
Required by the overhaul plan §9.3.

## What you hold

- Your claim is a **transferable Sui object NFT** (`VaultPosition`). It can be
  held, transferred, listed, or used anywhere Sui objects go — the vault never
  restricts transfer of a wallet-held position.
- The NFT carries its own **cost basis and lockup**. Performance fees are
  charged when the position exits, on the gain above ITS recorded basis. A
  secondary buyer **inherits the seller's embedded fee liability**; paying a
  market price for the NFT does not reset its on-chain basis. The UI shows
  both current estimated value and embedded basis/fee before any transfer.
- Splitting or merging positions never creates or destroys shares or basis;
  merging takes the LATER lock expiry.

## Whitelist scope (compliance consequence — plan §8.1)

The ingress whitelist gates **creation and deposits only**. Secondary
transfers and redemptions are NOT whitelist-gated: a non-whitelisted party
can buy a position on the secondary market and redeem it through the queue.
The whitelist bounds who can *create* exposure, not who can *hold or exit*
it.

## Tranches (senior/junior vaults only)

- **Senior hurdle returns are a priority claim, not guaranteed yield.** The
  hurdle grows what senior is entitled to receive before junior; if total
  assets fall below the senior claim after junior is exhausted, **senior
  loses money**. The claim is cumulative and keeps accruing during
  impairment (arrears absorb later recovery first).
- **Junior absorbs first loss** and owns the residual upside per the vault's
  immutable upside mode (preferred-only, capped participating, or uncapped
  participating — check the vault's terms; participation reduces junior's
  residual).
- **Coverage breach** (junior buffer below maintenance): junior withdrawals
  PAUSE (they stay queued in order); senior withdrawals keep flowing; the
  curator can only unwind, not deploy; new senior deposits stop. Below the
  higher target threshold, new senior deposits stop but nothing else changes.
- **Impairment** (junior wiped, assets below the senior claim): all ordinary
  deposits stop; only unwind, repayments, appraisals, and senior exits
  continue.
- **Junior reset**: after at least 7 days of persistent impairment AND 7 days
  of public notice, anyone may execute a reset by depositing enough fresh
  junior capital to cure the senior deficit and restore the target buffer.
  **A completed reset permanently wipes the old junior generation** — old
  junior NFTs become zero-value forever, even if NAV later recovers.
  Recovery before execution cancels the reset automatically.
  **Recapitalization value is used first to cure the senior deficit**; only
  the excess becomes new junior NAV.
- Exiting queued shares keep earning P&L (and senior accrual) until paid;
  queue order is FIFO within your tranche.

## Closure and settlement

- Closure settles **senior first**; within an underfunded tranche, holders
  share pro rata. Under terminal insolvency junior receives zero.
- After the one-time settlement snapshot, every position redeems directly
  against a frozen pool at any later time. **Unredeemed positions become
  perpetual claims on the settlement pool**; late redemption costs nothing
  but earns nothing further.
- Payout-asset preferences never outrank capital priority; terminal
  settlement pays the accounting asset.

## Curator and custody model

- The curator trades through audited, allowlisted adapters and can never
  withdraw vault funds to themselves. Entry/exit pricing consumes a complete,
  same-transaction oracle appraisal.
- The curator maintains a first-loss commitment escrowed INSIDE the vault
  (junior for tranched vaults), marked at ≥ the protocol minimum of NAV.
  Falling below it halts new deployment (not user exits) until cured.

## Risks

Oracle risk (mispriced marks move entry/exit value; haircuts damp but do not
eliminate it) · adapter/venue risk (each allowlisted adapter is an audited
but real attack surface) · liquidity risk (open-state withdrawals are
liquidity-constrained and not guaranteed; the aged accounting-asset fallback
is the backstop) · external-account risk (off-chain venue capital is
budget-bounded and attested, not custodied) · smart-contract risk (the vault,
adapters, and oracles are code; audits reduce, never remove, defect risk) ·
tranche risk as described above.
