# `options_vault` — DEPRECATED (SO-332)

The covered-call ("Ribbon-style") vault product is **retired**. This package
is no longer published to any network and nothing off-chain drives it.

The code stays in-tree deliberately: it is the reference for how the round
state machine, the share/pps accounting and the oracle-bounded cranks were
built. Do not extend it, and do not wire anything new to it. Its off-chain
counterparts (`crates/vault-sim`, `tools/backtester`, the keeper's
covered-call crank) were removed in SO-452; `vault_tests.move` is now the
only golden for the integer math.

## What "deprecated" means concretely

| | Before | Now |
|---|---|---|
| Publish | step 4 of the deploy pipeline | **not published** — removed from `deployment-manager` |
| Move CI | in the `move-ci.yml` matrix | **not built or tested** in CI |
| `deployments.json` | `packageInfo.vault` written each deploy | **absent** on fresh records; the field stays `Option` so old records still parse |
| Vault creation | `option-scheduler` vault-ensure pass | **off** — no `[vault_template]` in any shipped config |
| Cranks | `keeper` covered-call tick | **removed** (SO-452) — recover `keeper::legacy_vault` from git history |
| Events | 17 families indexed | **not subscribed** — the indexer's `vault` package id is `None` |
| Sponsorship | 5 `vault:*` gas-station templates | **not registered** |
| Read API | `GET /vaults*` on api-service | **unrouted** |
| APY | price-charting sampler + `GET /vault-apy/:id` | **not spawned**, **unrouted** |
| Frontend | `/vault` screen + nav tab | **redirects** to `/vaults` (curated trading vaults) |

## The kill switch that matters

`vault::pause_deposits` alone is **not** a decommission. The scheduler's
`vault_pass` retires a paused vault from its DB and rolls a *fresh
replacement* for that pair+cadence. The real switch is the absence of
`[vault_template]` (global and per-pair) in the scheduler config — that
leaves `vault_entries` empty and the pass never runs.

## If you ever need to run it again

1. Publish the package (`sui move build` still works — deps are `options_core`,
   `auction`, and Pyth) and put its id back in `deployments.json` under
   `packageInfo.vault`. Every consumer already reads it as optional, so the
   indexer, gas-station and scheduler light back up on their own.
2. Re-add `[vault_template]` to the scheduler config to provision vaults.
3. Restore the keeper's covered-call crank (`services/keeper/src/legacy_vault.rs`
   and its `keeper-legacy` binary, deleted in SO-452) from git history to
   crank them.
4. Re-route the api-service `/vaults` handlers, the price-charting
   `/vault-apy` route + sampler, and the frontend `/vault` screen.

## Successor

The live curated-vault product is [`contracts/trading-vault`](../trading-vault),
designed in [`docs/vault-curator-product.md`](../../docs/vault-curator-product.md).
It is not a drop-in replacement — different custody model, different share
accounting, curator-driven rather than rule-driven.
