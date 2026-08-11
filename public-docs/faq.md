# FAQ

### What kind of options does Pismo Protocol support?

American-style covered calls (and cash-secured puts, which mirror the covered-call design with cash collateral). "American-style" means holders can exercise any amount at any time before expiry. Every option is fully collateralized on-chain.

### Where do prices come from?

From competing market makers. When you request a quote, it's broadcast to all connected makers, and you see their signed responses sorted best-first. You always know the exact price before you sign.

### Can I sell an option after buying it?

Options are ordinary fungible Sui coins, so they're freely transferable and tradeable anywhere Sui coins trade. Positions (the writer side) are also transferable objects.

### What happens if I write a call and it gets exercised?

You sold the upside above the strike. After expiry you redeem your Position and receive `strike × exercised amount` in the settlement asset for the exercised portion, plus any unexercised underlying back. You keep the premium regardless.

### How do I know whether I'll be exercised before someone else?

Assignment is strictly first-in-first-out by write order. Your Position records your exact range in the queue, and the app shows the bucket's current exercise cursor, so you can always see precisely how much of your range is behind it.

### What are the fees?

The protocol takes a small percentage of the premium (configured in basis points on-chain; visible in the protocol config). Vaults additionally carry their own fee structure, visible per vault.

### Do I need SUI for gas?

Mostly no — the app's gas station sponsors protocol transactions built through the frontend.

### What happens to my vault deposit mid-round?

It's working: your share of the underlying is locked as collateral for the calls the vault sold this week. Withdrawal requests queue and are honored at the next round boundary.

### Is the code audited?

The contracts are structured into four small packages with one-way dependencies (core, auction, RFQ adapters, vault) specifically to keep the audited surface small and reviewable. Ask in the community channels for the current audit status.
