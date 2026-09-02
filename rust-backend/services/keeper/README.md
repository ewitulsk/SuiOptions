# keeper — trading-vault liveness layer

The keeper is the permissionless crank for the curated trading vaults
(`contracts/trading-vault`). It holds **only a gas wallet**; every action
it submits is validated on-chain, so a malicious keeper can at worst
waste its own gas and a lazy one can at worst delay a crank. N keepers
can run concurrently and merely race.

> The covered-call ("Ribbon-style") vault crank this crate used to carry
> (`legacy_vault`, `planner`, `slicing`, `state`, `strike`, `submit`, the
> `keeper-legacy` binary and the strike goldens shared with
> `crates/vault-sim`) was deprecated in SO-332 and deleted in SO-452
> together with `crates/vault-sim` and `tools/backtester`. The original
> spec is in git history before that ticket; the product record is
> [`contracts/.deprecated/vault/DEPRECATED.md`](../../../contracts/.deprecated/vault/DEPRECATED.md).

## What it does

Every tick (`tick_secs`, default 15) `trading_vault::tick` discovers the
vaults from the indexer's `trading_vaults` view and, per vault:

1. settles finished RFQ auctions,
2. redeems expired option positions,
3. sweeps DeepBook settled amounts and `vault_mm` transfer-ins into custody,
4. force-unwinds when the oldest withdrawal head has aged past the grace period,
5. posts external-account equity into the `EquityBook` (`venue_equity`),
6. fulfills the withdrawal lanes with a composed attestation-bearing appraisal,
7. runs `crank_capital` at `mark_refresh_interval_ms` cadence when idle,
8. drives terminal settlement for Closed vaults.

A read-only reconciliation monitor (`hedge-reconciliation`) and a
capital-state monitor (`tv-*` alerts) run alongside. The module doc at
the top of `src/trading_vault.rs` is the authoritative description.

## Layout

```
services/keeper/
├── src/
│   ├── main.rs           # boot (config, secrets, token-info, oracle) + tick loop
│   ├── lib.rs            # Cli + cli-spec program definition
│   ├── config.rs         # endpoints, Pyth handles, [vault_defaults], [external]
│   ├── discovery.rs      # Pyth state table → PriceInfoObject lookup
│   ├── trading_vault.rs  # the per-tick crank pass
│   └── venue_equity.rs   # external-venue equity sources (Bluefin) + clamping
└── config/               # config.{example,staging,prod}.toml (shipped in the image)
```

## Config

Vaults are discovered, not configured. The TOML carries the indexer
endpoint, the Pyth/Wormhole deployment handles (testnet `PriceInfoObject`s
are keyed by the BETA feed set, so `hermes_url` must be hermes-beta
there), `[vault_defaults].vol_window_days` for the VolBook mark crank,
and the optional `[external]` block (reconciliation thresholds, equity
posts, `[external.bluefin]` reader). See `src/config.rs` for every field
and `config/config.example.toml` for a local-dev starting point.

Spot and realized vol come from oracle-service (`--oracle-url`); the
keeper only hits Hermes directly for the on-chain VAA. Protocol ids come
from token-info (`--token-info-url`).

## Running

```
cargo run -p keeper -- --config services/keeper/config/config.toml \
    --secrets services/keeper/config/secrets.toml --network testnet
```

`Dockerfile.keeper` builds the deployed image; compose passes
`TOKEN_INFO_URL` / `ORACLE_URL` and mounts the gas key at
`/run/secrets/keeper.toml`. Health is served on `health_addr` (8086).

## Tests

`cargo test -p keeper` — config parsing of the shipped per-env TOMLs,
venue-equity clamping against a mock Bluefin endpoint, and the
trading-vault plan/appraisal unit tests. The live-testnet Pyth table
lookup is `#[ignore]`d (`cargo test -p keeper --lib -- --ignored discovery`).
