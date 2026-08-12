# `options_rfq` — DEPRECATED

The call/put RFQ adapters over the generic `auction` venue are
**retired** along with that venue (see `../auction/DEPRECATED.md`).
This package is no longer published to any network.

The code stays in-tree as the reference for the address-routed RFQ
settle flows. Do not extend it, and do not wire anything new to it.

## What "deprecated" means concretely

| | Before | Now |
|---|---|---|
| Publish | published after core | **not published** — removed from `deployment-manager` |
| Move CI | in the `move-ci.yml` matrix | **not built or tested** in CI |
| `deployments.json` | `packageInfo.rfq` written each deploy | **absent** on fresh records; the field stays `Option` so old records still parse |
| Auction opening | mm-bot `[sim]` retail stand-in | **idle** — the sim's auction opener no-ops when token-info carries no rfq id |
| Events | 6 families indexed | **not subscribed** — the indexer's `rfq` package id is `None` |
