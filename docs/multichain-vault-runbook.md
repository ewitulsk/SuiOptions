# Multichain Vault Ops Runbook

Companion to docs/multichain-vault-plan.md. Covers deploy order and the
incident levers for the hub/spoke lanes. All hub admin actions require
the `options_core` `AdminCap` (and `CuratorCap` where co-signed);
spoke-side roles per plan §6.1.

## Deploy order (testnet set first; mainnet is the same with the prod
## config set — plan §9)

1. **Hub packages** — fresh publish of `trading-vault-v2` (LAYOUT
   INCOMPATIBLE with prior deployments — this is a redeploy, not an
   upgrade; see API_DELTA.md), then `endpoint-layerzero` and
   `endpoint-ccip` (their pinned LayerZero/CCIP deps must match the
   on-chain packages for the target network — verify the pins against
   the live package ids before publishing). Record ids in
   `rust-backend/deployments.json`.
2. **Endpoint registry seeding** (hub, AdminCap):
   `endpoint::set_hub_chain_id`, `allow_endpoint<RelayerEndpoint>` (dev
   only — NEVER on mainnet), `allow_endpoint<LzEndpoint>`,
   `allow_endpoint<CcipEndpoint>`; `add_relayer` for the messenger's
   Sui address (dev endpoint only).
3. **EVM spoke** — deploy `TUSDG` (testnet only), `SpokeVault` (+
   endpoints: `RelayerEndpoint` dev / `LayerZeroEndpoint` /
   `CCIPEndpoint`), grant roles, fund the fee pot. Fill addresses into
   the frontend spoke config and vault-messenger config.
4. **Transport wiring** — LayerZero: `endpoint_lz::map_chain` (protocol
   chain id ↔ eid) + `set_peer` (spoke `LayerZeroEndpoint` address,
   left-padded), and the mirror peer/eid config on the spoke contract.
   CCIP: `endpoint_ccip::register_receiver` BEFORE any lane traffic
   (an unregistered receiver's messages finalize with no retry), then
   `map_chain` (chain id ↔ selector + remote endpoint), mirror config
   spoke-side.
5. **Bind** — `multichain::bind_spoke<Transport, USDG>` (AdminCap +
   CuratorCap) with `max_sync_age_ms`, `ack_deadline_ms` (MUST be
   strictly less than the spoke's `DEPOSIT_TIMEOUT` minus clock-skew
   margin), and the curator's spoke address.
6. **Oracle** — Switchboard USDG/USD feed live; pin the marker in
   `OracleRegistry` (`pin_oracle`); confirm the crank includes the
   spoke leg.
7. **Services** — `vault-messenger` (per its README: DB, secrets, ECR
   entry, deploy.sh) plus the appraisal crank config gaining the spoke
   leg. Smoke: deposit → ACK → active; withdraw → ACK → paid → receipt;
   `SpokeStateSynced` flowing.

## Incident levers

- **Freeze one spoke's curator activity**: hub — flip risk state via
  pause (`registry::set_paused` for protocol-wide, or vault
  `set_deposits_paused`), then crank `build_config_sync`; spoke-local
  break-glass — `PAUSER_ROLE` pause on `SpokeVault` (halts deposits and
  payouts; use for contract-level emergencies only).
- **Endpoint switch (LZ ⇄ CCIP)**: verify the standby transport's
  wiring (peer/selector config both sides), then rebind the spoke's
  endpoint hub-side and crank ConfigSync THROUGH THE OLD endpoint so
  the spoke learns the new endpoint code. Do this only with the lane
  quiet (no pending un-ACKed messages: check `GET /lanes` on
  vault-messenger).
- **Stuck spoke→hub message** (lane not advancing): check
  vault-messenger `GET /messages?status=failed`; a `bad_sequence` abort
  that persists means a gap — replay from the spoke's event log in seq
  order. A malformed-attestation abort: fix the oracle feed and retry
  (aborts do not advance the lane; nothing is lost).
- **`SpokeStateSynced.divergent = true`** (spoke reports MORE than hub
  books): stop and reconcile before anything else — this means funds
  the hub never minted against. Freeze curator activity, diff spoke
  events vs hub `SpokeDepositProcessed`/`SpokePayoutSettled`, find the
  unapplied or double-applied message.
- **`SpokePayoutSettled.unmatched > 0`**: books drifted on a receipt;
  same reconciliation as above (lower severity — clamped, NAV safe).
- **Payout queue aging** (`vault-messenger-payout-queue-aged`): spoke
  lacks funds; new deposits drain it FIFO. Pre-rebalance-integration
  there is no other lever (plan §5.1 accepted consequence) — inform the
  curator, surface queue depth to users.
- **Fee pot low** (`vault-messenger-fee-pot-low`): anyone can top up —
  `SpokeVault.fundFees()` from the ops wallet. Deposits/withdraw
  requests revert with `FeePotInsufficient` when empty; nothing is
  escrowed on a revert.
- **Dark spoke** (appraisals abort `spoke_stale` 145): NAV is blocked
  vault-wide by design. Restore the StateSync path (spoke RPC, relayer,
  transport) — do NOT widen `max_sync_age_ms` as a workaround during an
  incident; that trades safety for liveness exactly when it matters.
- **Deposit past ack deadline**: hub rejects (code 6), spoke refunds on
  the negative ack, or the user `reclaim()`s after `DEPOSIT_TIMEOUT`.
  A late ACK for a reclaimed seq raises `AlarmAckForReclaimed`
  spoke-side and needs manual review (should be impossible while
  `ack_deadline_ms < DEPOSIT_TIMEOUT − skew`).
- **Drain/unbind** (required before vault close): pause deposits, let
  withdrawals + receipts drain holdings and payables to zero, then
  `multichain::unbind_spoke`. `initiate_close` aborts 151 until every
  spoke is unbound.
