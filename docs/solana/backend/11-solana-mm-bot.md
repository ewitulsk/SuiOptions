# solana-mm-bot (`services/solana-mm-bot`)

Market-maker bot: auction bidder + signed-quote responder. **Standalone
workspace**. Holds a funded wallet + an ed25519 quote-signing key.

## Strategies (ported set)

1. **WS signed-RFQ quoting** against solana-quoting-service: connect → auth
   (sign 32-byte challenge with the quote key) → on `RFQBroadcast`, resolve
   bucket pricing via **solana-api-service** (`bucket_pricing` — never trust
   wire inputs), price with the shared `pricing` crate (vol-space spreads,
   smile, ttl charge — `pricing.rs` ports verbatim), sign the **Borsh** quote
   bytes ed25519, respond. Bulk-view unsigned quotes unchanged.
2. **Auction bidders** (the venue replaces Sui's per-kind auctions with one
   program, so ONE bidder module with mode-specific pricing):
   - `covered_call` auctions: price the slice as buyer (`side=Writer`
     economics), `decide_bid` ports verbatim (reserve, ceil min-increment via
     `options_math::min_next_bid` — path-import the real function, no
     re-derivation), InitialBidPolicy, rebid-when-outbid, escrow cap,
     deadline lead guard.
   - `cash_secured_put` auctions: put pricing leg (intrinsic floor), same
     decide_bid.
   - `swap` auctions: `max_underlying_bid = amount_s / spot × (1 − margin)`,
     paused-vault filter via solana-api-service.
   - Discovery: solana-indexer `auctions(status: open, mode: …)` — replaces
     both the api-service `/rfqs` poll AND the Sui event-walking for swaps
     (one uniform source). Deadline/best-bid state read from the `Auction`
     account via RPC just before bidding (freshness).
   - Bid submission: venue `bid` ix with `previous_bidder_refund` = derived
     ATA of the current best bidder (read from the auction account);
     `token_recipient` = our wallet ATA.
3. **No DeepBook quoter** (no order book on Solana yet — module not ported).

## Chain plumbing

- solana-tx: keypair, RPC wrapper, ix builders from program crates, ATA
  ensure-idempotent helper, submit+confirm.
- Bootstrap: ensure `MmAccount` PDA exists (`create_account(salt=0,
  scheme=0, quote_pubkey)`), ensure ATAs, deposit inventory
  (`account_deposit`) — for the quote flow the MM's funds sit in the
  MmAccount ATAs; auction bids fund from the wallet's own ATAs (venue
  escrows from bidder ATA).
- Replenish task: non-mainnet, `POST /faucet` on solana-gas-station? No —
  the bot holds its own keypair; it mints via... the faucet authority is the
  gas-station key, so the bot calls the gas-station faucet endpoint like any
  client (HTTP), or ops funds it manually. **Decision: HTTP faucet call**,
  keeps mint authority in one place.
- Benign-vs-alert classification: venue `BidTooLow` / `AuctionEnded` /
  outbid races are benign (`warn!`/`debug!`); everything else
  `error!(alert_id = "tx-failed-solana-mm-bot-<flow>")` with flows
  `quote|auction|swap`.

## Quote signing

`SolanaQuote` Borsh struct in solana-tx, golden-vectored against
`options_core::quote::Quote`. Sign ed25519 with `[mm_bot] quote_key`
(32-byte seed, base58 or hex). `signing_scheme = 0` registered on the
MmAccount. The **executor** (retail frontend) builds the Ed25519SigVerify
precompile ix — the bot only produces the detached signature.

## Config / secrets

- Same shape as Sui: `network`, `quoting_url` (ws://solana-quoting:9002),
  `underlying_symbols` (empty = derive from solana-token-info Pyth-fed
  tokens + underlying watcher restart pattern), `settlement_symbol` (TUSDC),
  spread knobs, `[smile]`, `[pyth]` staleness guards, `[onchain_auction]`
  (unified section: per-mode enable + decide_bid knobs + escrow caps),
  `[onchain_swap.bidder]` margin.
- CLI: `--token-info-url`, `--oracle-url` (solana-oracle), `--api-url`
  (solana-api-service), `--secrets`.
- Secrets `options/<env>/solana-mm-bot`: `[solana]` wallet keypair,
  `[mm_bot] quote_key`, rpc override. Ops metrics port 9010.

## Verification

- decide_bid/pricing/spot-cross unit tests port (pure). Quote golden vectors
  vs program crate. Auction-view parsing from fixture accounts.
- litesvm integration: create auction (as a fake vault/creator) → bot bids →
  outbid → rebid → settle; asserts refund-ATA handling.
