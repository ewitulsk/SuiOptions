---
description: "Frequently asked questions about Pismo Options, Pismo Exchange, and Pismo Vaults."
---

# FAQ

## Pismo Options

### What kinds of options are supported?

American-style **covered calls** and **cash-secured puts**. American-style means holders can exercise any amount at any time before expiry. Every option is fully collateralized on-chain — calls by the underlying asset, puts by the full strike value in cash — with no margin and no liquidations.

### Where do prices come from?

From competing market makers. When you request a trade, your request is broadcast to all connected makers, and you see their signed, executable responses sorted best-first. The price you accept is re-verified on-chain — you always get exactly the price you saw, or the transaction doesn't execute.

### Can I sell an option after buying it?

Yes. Options are ordinary fungible Sui coins — freely transferable and tradeable anywhere Sui coins trade, with [Pismo Exchange](exchange/overview.md) as the natural venue. Writer-side Positions are transferable objects too.

### How do I know whether I'll be assigned before other writers?

Assignment is strictly first-in-first-out by write order. Your Position records your exact range in the queue, and the app shows the bucket's exercise cursor — you can always see precisely how much of your range is behind it. No randomness, no operator discretion.

### What happens if I write a call and it gets exercised?

You sold the upside above the strike. After expiry you redeem your Position and receive `strike × exercised amount` in the settlement asset for the exercised portion, plus any unexercised underlying back. You keep the premium regardless. Writing a put mirrors this: assignment delivers you the underlying at the strike, paid from your cash collateral.

### Do options exercise automatically at expiry?

No. Exercise is the holder's action, any time before expiry. An in-the-money option that is never exercised expires worthless — set a reminder.

## Pismo Exchange

### How can quoting be free?

Orders are signed messages, not on-chain objects. Placing, repricing, and cancelling happen off-chain at zero cost; the chain is only involved when a fill settles. Fees are charged only on fills (*rates: information coming soon*).

### If the order book is off-chain, what stops it from cheating?

Every fill must carry the maker's signature over the exact terms — price, size, market, expiry — and the settlement contract re-verifies all of it on-chain before funds move. The service can match or not match; it cannot misprice, forge, or touch escrow. See [Limitations & Trust](exchange/limitations-and-trust.md).

### Can I trade on it through an aggregator?

That's the point. The taker-side fill is an ordinary composable call — coin in, coins out, no account needed — so swap routers can include Pismo Exchange orders in multi-hop routes like any AMM pool.

### Is my money locked while my orders rest?

No. Escrow withdrawal is owner-only and instant, and keeps working even if the exchange is paused. Resting orders are claims against your balance, not locks on it.

## Pismo Vaults

### Can the curator steal my deposit?

No — and this is a contract guarantee, not a promise. No vault function pays vault funds to the curator; all trading flows return proceeds to the vault; the one external channel is budget-capped, destination-locked, and jointly key-controlled. What the curator *can* do is lose money trading. Read [The Curator Security Model](vaults/curator-security-model.md) and [For Depositors](vaults/for-depositors.md).

### Who is the curator?

Anyone can create a vault and curate it. The app distinguishes **trusted curators** (vetted by Pismo — including the flagship vault Pismo itself curates to bootstrap options liquidity, a role we intend to hand to a professional market-making firm) from **public vaults** (anyone else, unvetted, same on-chain protections).

### How fast can I withdraw?

Withdrawals queue and are fulfilled against fresh valuations — normally quickly, but your capital is working, so it isn't instant. If the queue ever starves past the vault's grace period, permissionless force-unwind unlocks and anyone can free the funds. Your shares keep earning (and losing) until the moment you're paid.

### What are the fees?

Curators charge a performance fee on your individual realized profit, crystallized when you withdraw; the protocol takes a cut of the curator's fee, never a separate charge on you. Rates: *information coming soon*.

## General

### Is Pismo live on mainnet?

Not yet — everything currently runs on Sui testnet, and mainnet is being actively worked toward.

### Do I need SUI for gas?

Mostly no — the app sponsors most protocol transactions through a gas station, so you can trade without managing gas balances.

### What happens if Pismo's servers go down?

Trading pauses; custody doesn't. Options can still be exercised and redeemed, exchange escrow withdrawn, resting orders filled directly on-chain, and vaults force-unwound and drained — all permissionlessly. See [Architecture](infrastructure/architecture.md) for the full failure-mode table.
