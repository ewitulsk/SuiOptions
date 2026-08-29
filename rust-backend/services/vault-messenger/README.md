# vault-messenger

Hub↔spoke message relay for the multichain trading vault
(docs/multichain-vault-plan.md §8). An **initiator and fee payer, not a
trust gate**: content authenticity is the transport's job (the dev
relayer's sender gate in MVP; LayerZero DVNs / Chainlink CCIP DON later),
and the hub's `multichain` module re-checks lane wiring + sequence on
every applied message.

## What it does

- **Watches the EVM spoke** (Robinhood chain) for the endpoint's
  `OutboundMessage(bytes)` logs — the spoke→hub wire messages
  (`DepositNotice`, `WithdrawRequest`, `PayoutReceipt`, `StateSync`) —
  and persists each into a per-lane ordered Postgres queue keyed by
  `(direction, spoke_id, seq)` (unique — duplicate observations are
  suppressed). Statuses: `pending → submitted → confirmed`, terminal
  `failed`.
- **Delivers spoke→hub messages in seq order** (out-of-order arrivals are
  held back until the gap fills), one message per PTB:
  - `DepositNotice` / `WithdrawRequest`: oracle price legs (live
    provider + feed set from oracle-service `/oracle/descriptor` +
    `/oracle/legs`, mirroring the keeper's composer) →
    `vault::begin_appraisal` + holdings legs →
    `multichain::record_spoke_state` (spoke marker attestation) →
    `endpoint_relayer::deliver(bytes)` → `multichain::handle_*` →
    `endpoint_relayer::send` for the returned ACK.
  - `PayoutReceipt` / `StateSync`: `deliver` → `handle_*` (no appraisal).
  - Retries use capped exponential backoff; a `bad_sequence` (Move abort
    143) is classified "already applied → confirmed" only after
    re-reading the on-chain `Spoke.inbound_seq`.
- **Watches hub `OutboundMessage` events** (hub→spoke: `DepositAck`,
  `WithdrawAck`, `ConfigSync`) and, on the dev-relayer transport, submits
  the bytes to the spoke `RelayerEndpoint.deliver` with the EVM service
  account, same ordering/backoff/status discipline. On LayerZero/CCIP
  lanes (`transport != "dev-relayer"`) it submits nothing and confirms by
  watching the spoke's `lastInboundSeq()` advance.
- **Cranks**: the spoke's permissionless `syncState()` every 5 min
  (config) and hub `multichain::build_config_sync` +
  `endpoint_relayer::send` every 15 min — plus immediately on observed
  hub pause/risk/identity events (`hub.config_sync_event_types`).
- **Alerts** (`crates/observability` alert_id convention):
  - `tx-failed-vault-messenger` — terminal delivery failures and
    repeatedly-failing cranks (error level, per docs/tx-alerting.md;
    benign bad-sequence races are suppressed).
  - `vault-messenger-queue-stalled` — oldest undelivered message older
    than `queue_stalled_after_secs` (warn).
  - `vault-messenger-payout-queue-aged` — a `SpokeWithdrawProcessed`
    payable outstanding past `payout_aged_after_secs` with no
    `SpokePayoutSettled` (warn).
  - `vault-messenger-fee-pot-low` — spoke fee pot (from
    `SpokeStateSynced`) below `fee_pot_low_wei` (warn).
- **HTTP API** (port 9021, `/staging/vault-messenger` behind nginx):
  - `GET /health`
  - `GET /messages?spoke_id=&status=&limit=&offset=`
  - `GET /lanes` — per-lane last confirmed seq + queue depths + fee-pot
    report.

## Config

`config/config.toml` (dev), `config.staging.toml` (testnet-set),
`config.prod.toml` (mainnet-set) — one `network_set` selector per profile
(plan §9); promotion is a config/deploy flip. Hub package/object ids and
spoke addresses are placeholders until the phase-2/3 deployments publish.

Secrets (rendered to `/run/secrets/vault-messenger.toml` by
`deployment/ec2/render-secrets.sh` from `options/<env>/vault-messenger` =
`{"sui_key": ..., "evm_key": ..., "grpc_url": ...}`):

```toml
[sui]
testnet  = "suiprivkey1..."   # hub submitter — must be registered via
mainnet  = "suiprivkey1..."   # endpoint::add_relayer and hold gas
grpc_url = "https://..."      # REQUIRED (no public-fullnode fallback, SO-320)

[evm]
private_key = "0x..."         # spoke submitter — holds RELAYER_ROLE on
                              # RelayerEndpoint and native gas
```

## Deployment registration

Registered like cctp-relay in: workspace `Cargo.toml`,
`Dockerfile.vault-messenger`, `deployment/bake.hcl`,
`deployment/affected.py`, `deployment/ec2/deploy.sh`,
`deployment/ec2/render-secrets.sh`, `deployment/compose/
docker-compose.staging.yml` (prod: NOTE only, like cctp-relay),
`deployment/nginx/nginx.staging.conf` (+ prod comment).

**Manual steps this change does NOT make:**

- `rust-backend/infra/ecr.tf`: add `"vault-messenger"` to
  `local.service_repos` and run `terraform plan` + a targeted apply
  (`-target 'aws_ecr_repository.svc["vault-messenger"]' -target
  'aws_ecr_lifecycle_policy.svc["vault-messenger"]'`) so the ECR repo
  exists before the first bake pushes.
- Shared RDS: create the `vault_messenger_staging` role + database
  (migrations are embedded and run at boot).
- AWS Secrets Manager: create `options/staging/vault-messenger`.
- docs/tx-alerting.md keeps the registry of live alert ids — add
  `tx-failed-vault-messenger` there when this service first deploys.

## Testing

`cargo test -p vault-messenger` — network-free unit tests: lane ordering
(out-of-order held back), duplicate suppression, backoff + failure state
machine (incl. the bad_sequence re-check via mocked chain traits), wire
decode via `vault-messages`, ABI encode/decode for the EVM leg, event-json
parsing, and alert thresholds. No live-chain integration tests by design;
the e2e pass is plan §10 phase 4.

## Known gaps / to verify against a live deployment

- **PTB call shapes are built from the Move sources, not a live run**:
  `endpoint_relayer::deliver/send` arities, `multichain::handle_*`
  argument order, and the `record_spoke_state` `vector<PriceAttestation>`
  leg must be verified on the first testnet delivery (phase-4 e2e).
- **Appraisal completeness**: the composer covers what the keeper's
  covers (free balances, adapter positions, external equity via the
  optional adapter ids). A hub vault holding position types newer than
  `sui_tx::tx::appraisal` wedges the valuation-bearing handlers exactly
  like the keeper would.
- **Pyth price legs are not wired** — the deliverer requires the
  Switchboard pin (guaranteed available per the plan; USDG/USD Pyth is
  unconfirmed anyway). If the registry pins Pyth for the spoke marker,
  extend `HubClient::compose_valued_prefix` with the keeper's Pyth path.
- **EVM fee logic is legacy-tx with a 2× gas-price headroom** and a fixed
  `gas_limit`; fine for Orbit-class chains, revisit if the spoke chain
  needs EIP-1559 tip tuning.
- **hub→spoke lane on self-delivering transports** confirms via
  `lastInboundSeq()` polling only; per-message transport receipts
  (LayerZero executor status etc.) are phase-5 work.
- The Robinhood RPC URL + EVM chain id in the configs are placeholders.
