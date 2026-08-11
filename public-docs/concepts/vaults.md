# Covered-Call Vaults

Covered-call vaults automate the classic yield strategy: deposit an asset once, and the vault sells out-of-the-money calls against it every week, harvesting premium on your behalf.

## How a round works

Each vault runs a weekly cycle:

1. **Deposit.** Users deposit the underlying asset (e.g. SUI or wBTC) and receive vault shares.
2. **Strike selection.** At each roll, the vault targets the strike nearest **0.10 delta** — far enough out-of-the-money that calls usually expire worthless, close enough to earn meaningful premium.
3. **Auction.** The vault sells its calls through an **on-chain RFQ auction** — an open, escrowed ascending auction where market makers bid premium. The winning premium goes to the vault.
4. **Expiry & settlement.** After expiry the vault redeems its writer position. If the calls expired worthless, all underlying comes back. If they were exercised, the vault receives settlement asset instead, and converts it back to underlying through a second on-chain auction whose price is bounded by an oracle — so the conversion can't execute at a bad price.
5. **Roll.** The vault starts the next round automatically.

## Front of the queue, by construction

The vault writes its calls at the moment the week's buckets are created, so it always occupies the **front of the FIFO queue**. That means its worst-case outcome is exactly the textbook covered call — fully exercised at the strike, keeping the premium. Every other outcome (partial or no exercise) is strictly better.

## Permissionless cranking

The vault's entire round lifecycle — select strike, open the auction, settle it, redeem, convert proceeds, finalize the round — is driven by a **permissionless crank**. Anyone can advance the state machine; no privileged operator is needed for the vault to make progress. The protocol runs a keeper bot for convenience, but if it ever disappeared, anyone could turn the crank.

## Deposits, withdrawals, and fees

- Deposits enter the next round; your shares track your proportional claim on vault assets.
- Withdrawal requests are queued and honored at the round boundary (your assets may be locked inside an option position mid-round).
- Vault fees follow the standard structure for this class of product (management/performance), configured per vault and visible on-chain.

## Risks

A covered call caps your upside: if the underlying rallies far past the strike, the vault sells at the strike and keeps only the premium. The vault does not protect against the underlying falling — you keep price exposure to the asset you deposited, cushioned by the premium earned. Choose a vault only if you'd be comfortable holding the underlying asset outright.
