# evm-contracts — Multichain Vault Spoke (EVM)

Spoke-chain contracts for the multichain trading vault
(`docs/multichain-vault-plan.md`). The Sui hub owns ALL accounting (shares,
NAV, tranches, fees); these contracts are the deliberately **dumb spoke**:
they custody tokens, report raw facts, and execute hub instructions.

## Layout

| Path | What | Prod or mock |
|---|---|---|
| `src/lib/Wire.sol` | Codec for the 90-byte envelope + 7 payloads. The byte layout's source of truth is `rust-backend/crates/vault-messages`; `test/WireGolden.t.sol` pins this codec to that crate's golden fixtures. | production |
| `src/SpokeVault.sol` | Fund states (pending/active/reserved), depositor whitelist, deposit/reclaim, share-denominated withdraw + FIFO payout queue, message fee pot, curator integration interface, `handleMessage` lane (seq-ordered), roles (`AccessControlDefaultAdminRules` + WHITELIST/PAUSER), stale-heartbeat freeze. | production |
| `src/endpoints/LayerZeroEndpoint.sol` | Primary transport (OApp-style, peer pinned). | production |
| `src/endpoints/CCIPEndpoint.sol` | Secondary transport (CCIPReceiver-style, source pinned). | production |
| `src/endpoints/RelayerEndpoint.sol` | `RELAYER_ROLE`-gated endpoint. **Dev/CI only — never bind in production.** | dev/CI only |
| `src/interfaces/` | `IMessageEndpoint`, `ISpokeIntegration`, `ISpokeVault`. | production |
| `src/vendor/` | Minimal hand-vendored LayerZero V2 / CCIP interfaces (canonical upstreams noted in each file); call-compatible with the real endpoint/router. | production (interfaces) |
| `src/TUSDG.sol` | Open faucet-mintable 6-decimal USDG mock (mirrors `test-tokens` Sui faucets). | testnet only |
| `test/mocks/` | Mock LayerZero endpoint, mock CCIP router, mock integration. | test only |
| `lib/` | Vendored `forge-std` and OpenZeppelin v5.4.0. | — |

## Run tests

`forge` 1.5.x, solc 0.8.24 (pinned in `foundry.toml`):

```sh
forge build
forge test -vv
```

`test/WireGolden.t.sol` hardcodes the fixture bytes from
`rust-backend/crates/vault-messages/fixtures/*.hex`; if the wire layout ever
changes, regenerate the fixtures there (`BLESS_FIXTURES=1 cargo test -p
vault-messages`) and update the hex constants + all three codecs in one
commit.

## Key semantics (where they live)

- **Funds unusable until hub ACK**: `deposit` escrows into `pending`;
  only a `DepositAck` moves it to `active` (rejected → auto-refund;
  no ACK for `DEPOSIT_TIMEOUT` → depositor `reclaim`). A late ACK for a
  reclaimed/unknown seq emits `AlarmAckForReclaimed` and never bricks the
  lane.
- **Withdrawals are hub-directed**: the spoke records the request, asks the
  hub, and on `WithdrawAck` pays from `active` or queues FIFO (partial
  amounts reserved). While anything is queued, `extendTo` reverts and
  integration returns / `fundPayouts` donations service the queue before
  becoming active; ACKed deposits become `active` and drain the queue via
  permissionless `processPayoutQueue`. `WithdrawAck` carries no asset code,
  so payouts denominate in the constructor-set `PAYOUT_ASSET_CODE`.
- **Governance**: curator, active endpoint, and the integration set are NOT
  spoke roles — only hub `ConfigSync` changes them. The admin merely binds
  endpoint candidates (`bindEndpoint`); integrations install permissionlessly
  against `ConfigSync.integrations_root` (`setIntegrations`). Spoke roles are
  liveness-only: WHITELIST, PAUSER (break-glass pause of deposits+payouts),
  and RELAYER on the dev endpoint.
- **Fee pot**: outbound messages (deposit notices, withdraw requests,
  receipts, state syncs) are paid from a native pot (`fundFees()` /
  `receive()`); insufficient pot reverts with `FeePotInsufficient` before
  anything is escrowed.
- **Stale heartbeat**: no inbound hub message for `HEARTBEAT_TIMEOUT` ⇒
  `effectiveRiskOff()` = true locally (integrations freeze; deposits and
  payouts continue).
