# solana-keeper (`services/solana-keeper`)

Permissionless crank driver for options_vault. **Standalone workspace**
(solana-sdk + program crates + Pyth receiver). Holds only a funded gas wallet
— every crank is validated on-chain; zero privileges (identical trust model
to the Sui keeper).

## Architecture (ported shape)

- Tick loop (`tick_secs` 15): discover vaults from solana-indexer `vaults`,
  fetch each `Vault` account via RPC (`getAccountInfo` + anchor deserialize
  through the program crate — exact struct, no mirror), discover open
  auctions, run the **pure planner** `(VaultView, now_ms) -> Action`, submit
  one action per vault per tick. Stateless; restarts and lost races are
  harmless.
- Planner logic ports 1:1 (the phase machine is the same contract):
  Settling/expired: `CrankRedeem` → `SettleRfq`/`SettleRfqExpired` →
  `OpenSwapRfq`/`SettleSwapRfq` → `FinalizeRound`. Active: `SelectBucket` →
  `OpenRfq` slices → settle due auctions → mid-round swap. Slicing and
  strike-selection modules (`slicing.rs`, `strike.rs`) port verbatim — pure
  math over ms timestamps; `pricing::strike_for_delta`, iv_ratio, snap-up,
  GridCoverageMiss warning, clears_reserve, idle_or_finalize all unchanged.
- Strike candidates from indexer `buckets(underlyingMint, settlementMint,
  activeOnly)`; spot cross + realized vol from **solana-oracle-service** via
  `oracle-client` (path-imported; it's chain-agnostic).
- Auction discovery: indexer `auctions(status: open, creator: <vault pda>)` —
  much simpler than Sui's event-walking (the venue's `Auction` accounts are a
  queryable view). `vault.open_rfqs`/`open_swap_rfqs` counters cross-check.

## The Pyth leg (replaces Sui's in-PTB VAA prepend)

options_vault reads **`PriceUpdateV2`** accounts owned by the Pyth receiver
(`rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`), pinned feed ids,
`VerificationLevel::Full`, staleness/conf bounds. The keeper must place fresh
price accounts on chain before oracle-gated cranks:

1. Fetch the latest update for both feeds from **Hermes**
   (`pyth-client::latest_with_update_data` — the crate already exposes
   update-data bytes; path-import it).
2. Post via the **pyth-solana-receiver** flow: `post_update` writes a
   `PriceUpdateV2` into an **ephemeral keypair account** (keeper pays rent),
   using the wormhole-verified update. Use the `pyth-solana-receiver-sdk` /
   receiver program instructions from solana-tx.
3. Send the crank instruction referencing the two price accounts — **in the
   same transaction where size permits** (post_update × 2 + crank usually
   fits; if not, post first, crank second — accounts persist).
4. `close` the update accounts afterward to reclaim rent (or reuse one
   account per feed, overwriting — the receiver supports write-then-reuse via
   the same payer; v1: reuse two long-lived per-feed accounts to avoid
   rent churn entirely; fall back to ephemeral+close on decode drift).

**Decision**: maintain two persistent update accounts per (vault-feed pair
set), refreshed per oracle-gated crank. Cheaper and simpler than
create/close cycles. Failure points: Hermes down → cranks blocked (guards
already classify as Retry); receiver program upgrade changing layout →
`decode-failed`-style fatal alert.

## Submission & error classification (`submit.rs` port)

- solana-tx `submit_and_confirm`: build v0 message, sign, send via Helius RPC
  (`skipPreflight=false`), confirm at `confirmed`.
- Classify **Anchor error codes** from simulation/execution logs into the
  three classes with the same alerting contract:
  - **Benign** (lost race / state advanced): vault "wrong phase", "auction not
    over", "already settled", "nothing to redeem", venue `AuctionEnded`,
    `BidTooLow`-as-cranker etc. → `debug!`, replan next tick.
  - **Retry**: Pyth staleness/confidence errors, RPC/Hermes transients,
    blockhash expiry, unknown → `error!(alert_id = "tx-failed-solana-keeper",
    class = "retry")`.
  - **Fatal**: feed-id mismatch, config-invalid families → same alert with
    `class = "fatal"` + vault inserted into the halted set.
  The exact code lists come from `programs/*/src/error.rs` at implementation
  time and are unit-tested against the program crates' error enums (no magic
  numbers).

## Config / secrets

- `indexer_graphql_url`, `tick_secs 15`, `health_addr 0.0.0.0:8086`,
  `[pyth] hermes_url` (beta on devnet), receiver/wormhole program ids
  (constants with config override), `[vault_defaults]` iv_ratio /
  target_delta / short_round_target_delta / sigma_fallback / vol_window_days /
  `[slicing]` — all as Sui.
- CLI: `--token-info-url` (solana-token-info), `--oracle-url`
  (solana-oracle-service), `--secrets`, `--network`, `--dry-run`.
- Secrets `options/<env>/solana-keeper`: `[solana]` keypair (gas only) +
  shared rpc override; optional `[pyth] api_key` for Hermes.

## Verification

- Planner/slicing/strike tests port with ms fixtures (pure functions).
- Error-classification tests against program error enums.
- **litesvm end-to-end**: the program repo already tests with litesvm; the
  keeper adds an integration test spinning litesvm with the three programs,
  creating a vault + bucket, and driving one full round with the keeper's
  planner + real instruction builders (Pyth accounts stubbed with
  hand-written PriceUpdateV2 data — litesvm lets us write raw accounts).
  This is the highest-value test in the whole port.
