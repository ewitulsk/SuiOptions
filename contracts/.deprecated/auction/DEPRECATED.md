# `auction` — DEPRECATED

The generic escrowed ascending auction — the on-chain venue behind the
vault's RFQ premium-selling channel and the desk's `bid_on_auction`
BidTicket flow — is **retired**. The desk writes through the VaultMm
quote path now, and this package is no longer published to any network.

The code stays in-tree as the reference for the coupled-auction escrow
and settle-authority design. Do not extend it, and do not wire anything
new to it.

## What "deprecated" means concretely

| | Before | Now |
|---|---|---|
| Publish | first step of the deploy pipeline | **not published** — removed from `deployment-manager` |
| Move CI | in the `move-ci.yml` matrix | **not built or tested** in CI |
| `deployments.json` | `packageInfo.auction` written each deploy | **absent** on fresh records; the field stays `Option` so old records still parse |
| Dependents | `options_rfq`, `options_adapter` | `options_rfq` retired with it; `options_adapter` had its auction-coupled surface (tickets, RFQ flows, bid cranks) stripped |
| Bidding | mm-bot `[desk.auctions]` channel | **off** — config default flipped to `enabled = false` |
| Cranks | keeper ticket settle/reclaim/redeem | idle — no tickets can ever be minted |
| Events | 4 families indexed | **not subscribed** — the indexer's `auction` package id is `None` |
