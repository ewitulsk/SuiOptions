# Getting Started

This guide walks you through your first trade on Pismo Protocol.

## 1. Connect a wallet

Open the Pismo Protocol app and connect any Sui-compatible wallet. The app sponsors most protocol transactions through a gas station, so you can get started without worrying about gas for every click.

## 2. Get test funds

On testnet/staging deployments, use the built-in **Faucet** page to mint test tokens (test USDC and test underlying assets) straight to your wallet.

## 3. Browse contracts

The **Composer** shows the live option chain: each row is a bucket — an (asset, expiry, strike) contract — with live pricing from connected market makers. Fresh weekly expiries with a vol-aware strike grid are listed automatically every roll.

## 4. Write or buy a call

1. Choose a bucket and an amount.
2. Request quotes — competing market-maker prices appear within a couple of seconds.
3. Pick the best quote and sign a single transaction.

If you **wrote** the call: the premium lands instantly, and a Position object in your wallet records your place in the exercise queue.

If you **bought** the call: the option coins land in your wallet like any other Sui coin.

## 5. Manage positions

The **Dashboard** shows everything you hold:

- **Options you bought** — exercise any amount at any time before expiry (pay `amount × strike`, receive underlying).
- **Positions you wrote** — after expiry, redeem to collect your outcome: underlying back for the unexercised part, `strike × amount` in settlement asset for the exercised part.
- **Vault deposits** — share balance, current round status, and withdrawal requests.

## 6. Or just use a vault

If you'd rather not trade actively, deposit into a [covered-call vault](../concepts/vaults.md) and let it sell weekly calls for you.
