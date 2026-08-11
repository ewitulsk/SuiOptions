# Writing & Trading Options

Pismo Protocol is quote-driven: prices come from professional market makers competing to fill your request, and every fill settles atomically on-chain.

## The RFQ flow

1. **Pick a contract.** Browse the available buckets (asset, expiry, strike).
2. **Request a quote.** The app sends a request-for-quote (RFQ) over WebSocket to the quoting service, which broadcasts it to every connected market maker on the other side of your trade.
3. **Market makers respond** within a ~2-second window with signed quotes. The service validates each signature and checks the maker actually has the balance to back it, then returns the quotes sorted best-price-first.
4. **You pick a quote and sign one transaction.** The signed quote is embedded in your transaction; the chain re-verifies the maker's signature, checks the quote hasn't expired or been used before, and executes the trade atomically.

There is no order book to sit on and no partial-fill risk — either the whole trade executes at the quoted price, or nothing happens.

## Writing a call (earning premium)

You deposit the underlying asset into the bucket and immediately receive the premium (minus the protocol fee) from the market maker who bought your call. You get:

- a **Position** object recording your range in the FIFO queue, and
- the premium, paid instantly.

The market maker receives the option coins. After expiry, you redeem your Position for whatever your range earned: unexercised underlying back, and `strike price × exercised amount` in settlement asset for any exercised portion.

## Buying a call (paying premium)

You pay the premium; the market maker's account provides the underlying that collateralizes the option. You receive the option coins directly in your wallet.

## Exercising

American-style: exercise **any amount, any time before expiry**. You pay `amount × strike` in the settlement asset and receive that amount of underlying from the bucket. Partial exercise is just a coin split.

After expiry, unexercised options are worthless and can be burned.

## Quote safety

Every quote is signed by the market maker's registered key and includes:

- a **nonce** — each quote is executable at most once; replays are rejected on-chain,
- an **expiry** (typically 30–60 seconds) — stale quotes cannot be executed,
- the exact **bucket, amount, and premium** — nothing about the trade can be altered after signing.

The quoting service itself holds no funds and signs nothing; it is a routing layer. Even if it misbehaved, it could not spend or move anyone's assets.
