# Transaction Alerting & Error Messages

How to keep tx-failure alerting consistent across services. Pairs with the
`alert_id` convention in `crates/observability`.

## The rule

Any time a service submits a Sui transaction and it **fails**, log:

```rust
error!(alert_id = "tx-failed-<service>[-<context>]", error = %format!("{e:#}"), …, "…");
```

`error!(alert_id = …)` anywhere fires the provisioned Grafana rule, grouped by
`alert_id`. No infra change per alert.

## Where to put it

- Emit at the **service's failure handler** (where the submit `Result` is
  matched), **not** in the low-level `sui-tx` `submit_ptb` /
  `execute_transaction_block` — those bail on every revert, including benign
  ones, and would spam.
- Keep `alert_id` **low-cardinality**: one per service/operation. Put vault /
  object / pool ids in structured fields, never in the `alert_id` string.

## Suppress expected benign failures

Some reverts are normal and must **not** page:

- **keeper** `ErrorClass::Benign` aborts (lost a crank race) — stay `debug!`.
- **mm-bot** auction bids that abort with `rfq_bid_too_low` (Move code `31`,
  shared by `rfq` + `swap_auction`) — stay `warn!`. Use
  `mm_bot::is_benign_bid_loss`.

Everything else (transient/retry, fatal, gas, dry-run revert) gets the alert.

## Intentionally not alerted

- One-shot startup/bootstrap txs (e.g. mm-bot Account create) — a failed boot
  is loud via health checks.
- Best-effort cancels — resting orders self-expire on-chain.

## Current alert_ids

`tx-failed-keeper`, `tx-failed-option-scheduler`,
`tx-failed-option-scheduler-vault`, `tx-failed-mm-bot-rfq`,
`tx-failed-mm-bot-swap`, `tx-failed-mm-bot-deepbook`,
`tx-failed-gas-station-topup`.

When adding a new submission path, add a new `tx-failed-<service>` id here.
