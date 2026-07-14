# solana-oracle-service (`services/solana-oracle-service`)

Second deployment of the Pyth gateway for the Solana stack. The Sui
oracle-service contains **zero chain code** — its only Solana-relevant
difference is *which catalog it discovers feeds from* (solana-token-info
instead of token-info).

## Design decision: thin wrapper crate over `oracle-service` as a lib

`services/oracle-service` already has `src/lib.rs`. Refactor it minimally so
the boot path is callable (`oracle_service::run(config, secrets) -> Result<()>`
— move the body of `main.rs` into `lib.rs::run`), then
`services/solana-oracle-service` is a ~30-line crate:

- `main.rs`: parse the same CLI, load its own `config/config*.toml`
  (pointing `token_info_url` at `solana-token-info:9005`), call
  `oracle_service::run`.
- `observability::init("solana-oracle-service")`.

Zero logic duplication; feed discovery, Hermes SSE, PriceCache, BenchmarkVol,
router, WS fanout are shared byte-for-byte. Consumers use the existing
`crates/oracle-client` unchanged (chain-agnostic).

Alternative considered: one shared container with a second config — rejected
because the deploy machinery (tags, health paths, gatus, prometheus) is
strictly per-service; a thin crate is cheaper than bending the tooling.

## Specifics

- Port 9013 (own container `solana-oracle-service`).
- Feeds: every solana-token-info token with a `pyth_feed_id`. Pyth feed ids
  are chain-agnostic, so SOL/USD etc. resolve on the same Hermes endpoints.
- `hermes_url`: **hermes-beta** while the catalog carries beta feed ids
  (devnet parity with the Sui stack's testnet posture); prod-on-mainnet flips
  to stable Hermes + stable ids — config-only change.
- Secret `options/<env>/solana-oracle-service` (`pyth.api_key`, optional,
  same skip-if-placeholder semantics).
- Nginx: NOT exposed publicly today (Sui oracle-service isn't either) — it's
  internal; only `/health`+`/metrics` scraping matters. (Check: Sui
  oracle-service has no nginx block; keep parity.)

## Failure points (third-party: Pyth Hermes)

Same as the Sui stack: Hermes outage → prices stale → keeper/mm-bot guards
(staleness bounds) suppress activity rather than trade on bad data. Beta
Hermes is best-effort infrastructure; mainnet cutover to stable Hermes is the
long-term posture.

## Verification

- The shared lib keeps oracle-service's existing tests. New: a boot smoke test
  that `run` wires the solana config (feed discovery mocked via a stub
  token-info server).
