# Multichain Trading Vault: Sui Hub / EVM Spokes

Status: DESIGN — reviewed with Evan 2026-08-28 (Q&A captured below); not yet implemented.

## Summary

Extend trading-vault-v2 to a hub-and-spoke multichain vault:

- **Sui is the hub.** The existing `TradingVault` remains the single source of
  truth for ALL accounting: share supply, NAV, entry/exit haircuts, lockups,
  performance fees, withdrawal ordering. Nothing about hub-side deposits,
  sessions, or the capital machinery changes for single-chain vaults.
- **Every other chain is a spoke.** A spoke is a deliberately **dumb vault**:
  it custodies USDC, accepts deposits and withdrawal requests, and executes
  hub instructions. It does no share math. It has a per-spoke set of
  **integrations** that bound what the curator can do with funds on that
  chain (mirror of the hub's `IntegrationRegistry` philosophy).
- **Messaging is transport-agnostic.** Hub and spoke each speak to an
  endpoint abstraction; concrete transports (our own attestor, Wormhole,
  LayerZero) plug in per spoke. CCTP is NOT a transport — it is a **vault
  integration** the curator executes to move USDC between the hub and spoke
  vaults (and spoke↔spoke).
- **The curator holds keys on every supported chain** and interacts with a
  spoke the same way they do the hub: sessions/integrations only, never
  withdrawal.

MVP target: Sui hub + **Robinhood Chain spoke (deposits/withdrawals only)** +
**Hyperliquid spoke (deposits/withdrawals + vault-custodied HyperCore trading
via CoreWriter)**.

### Decisions from the 2026-08-28 review

1. Messaging: build the agnostic endpoint interface; integrate whichever
   concrete transports are necessary to cover Robinhood and Hyperliquid.
   CCTP is a curator-executed vault integration, not messaging.
2. Accounting: hub is master; spokes are dumb executors. All position-level
   accounting (shares, basis, fees, lockups) lives on the hub.
3. Withdrawals are in MVP scope, hub-directed: spoke tells the hub "user X
   wants to withdraw", waits, hub answers "pay user X amount A".
4. Hyperliquid trading is vault-custodied via CoreWriter + read precompiles
   (not the external-account pattern).

### External facts this design depends on (verified 2026-08-28)

- Robinhood Chain: Arbitrum Orbit L2, public testnet 2026-02-10, mainnet
  2026-07-01. **No Wormhole/LayerZero/CCTP support yet** → our attestor
  transport is required for this spoke, and curator rebalancing to/from
  Robinhood cannot use CCTP until Circle deploys there.
- HyperEVM: native USDC + **CCTP v2** live; Wormhole and LayerZero both live.
- Sui: **CCTP v1 only today**; Circle has said canonical (v2) contracts land
  on Sui ~end of H1 2026 and began phasing out v1 on 2026-07-31. HyperEVM is
  v2-only, so hub↔Hyperliquid CCTP is **gated on Sui CCTP v2 being live** —
  re-verify at implementation start.
- Robinhood Chain USDC: availability/flavor (native vs canonical-bridged
  USDC.e) unconfirmed — must be resolved before the Robinhood spoke config
  is finalized (open question §11).

## 1. Roles and fund states

Hub: existing `vault_v2::TradingVault`, extended with a spoke ledger (§3).
Multichain is restricted to **Untranched** vaults for MVP (§11).

Spoke vault fund states (USDC only for MVP):

| State      | Meaning                                            | Curator can touch? |
|------------|----------------------------------------------------|--------------------|
| `pending`  | deposit escrowed, no hub ACK yet                   | NO                 |
| `active`   | hub-ACK'd; part of vault NAV                       | via integrations   |
| `reserved` | owed to a hub-ACK'd withdrawal (payout queue, §5)  | NO                 |

The pending→active transition happens ONLY on a hub `DepositAck`. This is
the "funds unusable until ACK" invariant from the spec, enforced on-chain on
the spoke.

## 2. Messaging layer

### 2.1 Envelope

Every message: `{src_chain_id, dst_chain_id, src_app, dst_app, seq, payload}`.
`seq` is per (src, dst, app-pair) and strictly increasing; receivers keep
`last_seq` and reject replays/out-of-order delivery (messages are delivered
in order per direction; the relayer retries until landed).

Payloads (bcs on Sui / abi-encoded on EVM; one canonical byte layout defined
in the schema crate, §7):

Spoke → Hub:
- `DepositNotice { spoke_id, deposit_seq, depositor: bytes32, amount }`
- `WithdrawRequest { spoke_id, request_seq, user: bytes32, shares, all: bool }`
- `PayoutReceipt { spoke_id, request_seq, amount }`
- `BridgeReceipt { spoke_id, transfer_id, amount }` — CCTP funds arrived
- `StateSync { spoke_id, free, reserved, integration_equity, ts }` — NAV leg

Hub → Spoke:
- `DepositAck { deposit_seq, accepted: bool, shares, nav_per_share }`
- `WithdrawAck { request_seq, user: bytes32, pay_amount }`
- `ConfigSync { paused, risk_off, curator: address, integrations_root }`

### 2.2 Hub side (Move): endpoint adapter pattern

Mirror the existing oracle/integration adapter pattern: each transport is a
witness-typed module allow-listed in a new `EndpointRegistry`. A transport
module verifies its own proof (attestor signatures; later a Wormhole VAA)
and constructs a `VerifiedMessage` **hot potato** that only
`vault_v2::multichain` can consume. `multichain` checks the spoke binding
(`spoke_id → (chain_id, spoke_vault_address, endpoint_type)`) and `seq`,
then applies the payload. Outbound: `multichain` emits a canonical
`OutboundMessage` event; transports that need an on-chain send (Wormhole
publish) wrap it, the attestor transport just relays the event.

- `endpoint-attestor` (MVP, required for Robinhood): ed25519 signature(s)
  over `domain_tag ‖ envelope ‖ payload` verified against pubkeys in
  `EndpointRegistry` (same shape as the existing `registrar_pubkey`
  attested-registration flow). Start with 1 key, structure for k-of-n.
- `endpoint-wormhole` (phase 7): VAA verification via Wormhole's Sui core
  contract, for the Sui↔HyperEVM lane. Proves the abstraction with a second
  transport and removes our attestor from that lane's trust path.

### 2.3 Spoke side (EVM)

`SpokeVault` talks to an `IMessageEndpoint`:

```solidity
interface IMessageEndpoint {
    function send(bytes calldata envelopeAndPayload) external;   // outbound
    // inbound: endpoint verifies transport proof, then calls
    // spokeVault.handleMessage(envelope, payload); vault checks
    // msg.sender == registered endpoint + seq.
}
```

- `AttestorEndpoint` (MVP): ECDSA (k-of-n) verification, relayer-submitted.
- `WormholeEndpoint` (phase 7, HyperEVM only).

Endpoint per spoke is set at deploy and changeable only via hub `ConfigSync`
(+ timelock later; MVP: admin owner).

## 3. Hub accounting extensions (`vault_v2::multichain`)

New module(s) in the `trading-vault-v2` package (upgrade), keeping
`vault.move` changes minimal and behind `public(package)` helpers.

- **Spoke registry**: shared `MultichainRegistry` (admin-gated, like
  `IntegrationRegistry`): `spoke_id → SpokeConfig { chain_id, vault_address:
  bytes32, endpoint: TypeName, active }`, plus per-vault opt-in: a vault
  binds spokes via its `CuratorCap` + admin co-sign (matches
  `set_external_account` posture).
- **Spoke holdings ledger** (per vault, dynamic field):
  `Table<SpokeUserKey{spoke_id, user: bytes32}, Holding { shares: u128,
  basis: u64, deposited_at_ms: u64 }>` — same untranched share math
  (`SHARE_OFFSET`, floor division) and the same entry/exit haircuts, lockup,
  and exit-crystallized performance fee paths as hub `VaultPosition`s.
  Shares mint into the same `TrancheBook` supply, so NAV/share is global.
- **Per-spoke asset accounting** on the vault:
  `SpokeAssets { free, reserved, integration_equity, last_sync_ms,
  in_flight_out, payables }`.
  - Updated by `StateSync` (attested) and by bridge bookkeeping (§6).
  - **Appraisal extension**: `begin_appraisal` snapshots the set of bound
    spokes into the `Appraisal` hot potato (like `external_pending`); a new
    `record_spoke_state` leg per spoke (only the bound endpoint's transport
    may deliver it, freshness-checked against `max_price_age_ms`-style
    config) adds `free + reserved + integration_equity + in_flight` to
    `total_value`, minus `payables`. NAV cannot complete with a stale spoke
    → deposits/withdrawals/releases block chain-wide if a spoke goes dark,
    which is the safe failure mode.
- **Message handlers** (consume `VerifiedMessage`, require a complete
  `Appraisal` where valuation happens):
  - `handle_deposit_notice`: vault open + not paused → mint shares at
    current NAV with entry haircut into the ledger, bump `SpokeAssets.free`,
    queue `DepositAck{accepted: true, shares}`. Rejections (paused/closing)
    → `DepositAck{accepted: false}` so the spoke refunds.
  - `handle_withdraw_request`: validate holding + lockup, value shares at
    current NAV with exit haircut, crystallize perf fee vs basis (existing
    fee machinery; fee shares to the curator commitment position), **burn
    the shares**, `payables += pay_amount`, queue `WithdrawAck`.
  - `handle_payout_receipt`: `payables -= amount`, `free/reserved -= amount`.
  - `handle_bridge_receipt`: clear matching `in_flight_out` (§6).
- **Gate propagation**: `ConfigSync` pushed on every relevant hub state
  change (pause, risk-state flip from `sync_capital`, curator rotation).
  Spokes also treat a stale ConfigSync/heartbeat (> N minutes) as
  `risk_off = true` locally — curator integrations freeze, deposits and
  payouts continue.

## 4. Deposit flow (spoke)

1. User: `SpokeVault.deposit(amount)` → USDC escrowed `pending`, local
   `deposit_seq` assigned, `DepositNotice` sent.
2. Hub handler mints ledger shares (entry haircut, basis recorded,
   `deposited_at_ms` = hub clock) → `DepositAck`.
3. Spoke on ACK: `pending → active`; emits `SharesRecorded(user, shares,
   nav_per_share)` (spoke keeps a **non-authoritative mirror** of share
   balances for UX; hub ledger is truth). Only now is the USDC tradable.
4. Refund path: if no ACK within `deposit_timeout` (config, e.g. 24h), the
   depositor may `reclaim(deposit_seq)` — escrow returned, seq marked dead.
   The hub refuses to ACK a notice older than `deposit_timeout − margin`
   (chain-timestamped), so a late ACK for a reclaimed deposit cannot occur;
   an ACK for a reclaimed seq is rejected by the spoke and alarmed.

Shares on the spoke are non-transferable claims in MVP (no ERC-20; transfers
would need hub round-trips — future work, §11).

## 5. Withdrawal flow (spoke) — hub-directed

Spec framing confirmed: the hub is master; the spoke asks and waits.
Two refinements over the raw spec:

- **Requests are share-denominated** (`shares` or `all`), not
  dollar-denominated. NAV moves between request and ACK; a $-request can
  become unsatisfiable or mispriced in transit. The UI shows an estimated $
  from the latest NAV; the hub prices exactly at ACK time. (A $-request
  variant can be layered on later as "hub converts to min(shares_for($X),
  holding)".)
- **The spoke may be underfunded when the ACK arrives** (funds on HyperCore
  or bridged elsewhere), so the ACK feeds a payout queue rather than
  requiring instant payment.

Flow:

1. User: `requestWithdraw(shares | all)` → spoke records the request
   (blocks duplicate in-flight requests per user), sends `WithdrawRequest`.
2. Hub: validates against the ledger (authoritative), enforces lockup,
   prices at current NAV with exit haircut, crystallizes fees, burns
   shares, books `payables` → `WithdrawAck{user, pay_amount}`.
3. Spoke on ACK: if `free ≥ pay_amount` → pay immediately; else move what
   exists to `reserved` and enqueue the remainder (FIFO). **While the
   payout queue is non-empty, curator integration calls that move USDC out
   of the vault revert**, and all USDC arriving (integration returns, CCTP
   mints) services the queue before becoming `active`. The curator is
   expected to CCTP funds back; the queue gate makes that mandatory before
   trading resumes on that spoke.
4. On each payment: `PayoutReceipt` → hub clears the payable.

Rejected requests (lockup, no holding) get `WithdrawAck{pay_amount: 0}` →
spoke unlocks the request.

Hub-side FIFO lanes are untouched: hub lanes govern hub liquidity; each
spoke queue governs that spoke's liquidity. Cross-chain fairness between
queues is deferred (§11).

## 6. CCTP as a curator integration

Curator-executed USDC movement between **registered vault addresses only**.
Every transfer is tracked so NAV never loses or double-counts in-flight
funds: source books `in_flight_out{transfer_id, amount, dst}` at burn;
destination sends `BridgeReceipt` on arrival; hub moves the amount from
in-flight to the destination's balance (hub arrivals clear directly).

- **Hub outbound (Sui)**: extend the existing `cctp_bridge` package pattern
  with a vault-gated path: a curator `Session`-style `bridge_out<USDC>`
  takes from free balance (risk-off gated, budget-capped like
  `release_external`) and builds the burn ticket with `mint_recipient` =
  the spoke vault address; records in-flight.
- **Hub inbound (Sui)**: CCTP mints to an address → mint to **the vault
  object's own address** and use the existing
  `receive_coin<T, W>`/`Receiving` path from a crank session to sweep it
  into free balance and clear in-flight.
- **Spoke `CCTPIntegration.sol`**: `bridgeTo(dst_vault_id, amount)` —
  curator-only, active-funds-only, payout-queue-gated, recipient forced to
  the registered vault address for `dst_vault_id` (CCTP v2
  `depositForBurn`). Inbound: permissionless `receiveMessage` completion;
  vault credits on mint and emits `BridgeReceipt`.
- **Relay**: extend `rust-backend/services/cctp-relay` (today Sui↔Solana
  v1) with CCTP **v2** iris endpoints and EVM submitters so curator bridges
  auto-complete. Same DB/state machine.

Constraints (re-verify at build time): Sui v2 gates hub↔HyperEVM;
**Robinhood Chain has no CCTP**, so for MVP Robinhood-spoke funds stay on
Robinhood — deposits fund withdrawals locally, which matches its
deposits-only scope. If rebalancing off Robinhood becomes necessary before
Circle arrives, the only route is its canonical bridge (out of MVP scope).

## 7. Spoke contracts (new `evm-contracts/` Foundry workspace)

- `SpokeVault.sol` — fund states + deposit/reclaim + withdraw request/queue
  + `handleMessage` dispatch + endpoint binding + curator address (from
  `ConfigSync`) + integration registry + pause/stale-heartbeat freeze.
- `AttestorEndpoint.sol`, later `WormholeEndpoint.sol`.
- `integrations/CCTPIntegration.sol` (§6).
- `integrations/HyperCoreIntegration.sol` (HyperEVM only): the spoke's
  vault-custodied trading account. Holds the HyperCore account identity
  (its own contract address); curator-only, whitelisted CoreWriter actions:
  `toCore/fromCore` (USDC class/spot transfers between the EVM vault and
  HyperCore — always to/from the vault, never third parties), `placeOrder`
  / `cancelOrder` (encoded CoreWriter actions; MVP: perp + spot limit
  orders and cancels only — explicitly no withdraw-to-address, no
  delegate/agent approvals). `coreEquity()` reads positions/balances/marks
  via the read precompiles; it feeds `StateSync.integration_equity` and is
  verifiable on-chain. CoreWriter actions are async (executed on HyperCore
  a block later) — equity reads already reflect settled state, and the
  attested `StateSync` timestamp covers the gap.
- Shared message codec library mirroring the Move schema; a `schema`
  fixture set (golden byte vectors) keeps Move/Solidity/Rust codecs in
  lockstep.

## 8. Off-chain

- **New `rust-backend/services/vault-messenger`** (patterns: `cctp-relay`
  watcher + `hedge-signer` key handling): watches hub events and spoke logs,
  persists an ordered per-lane message queue in Postgres, attestor-signs,
  submits with per-chain relayer keys (AWS Secrets), retries with backoff;
  emits `error!(alert_id = "tx-failed-vault-messenger")` per the
  tx-alerting convention; also produces the periodic `StateSync`
  attestations (reads spoke `free/reserved` + `coreEquity()` via RPC) that
  the appraisal crank consumes. Full 9-spot deployment registration per
  repo convention.
- **cctp-relay v2/EVM extension** (§6).
- Appraisal/crank flow gains the spoke legs: crank tx = price attestations
  + `record_spoke_state` per spoke + existing legs.

## 9. Frontend

- Spoke deposit/withdraw screens with EVM wallet support (wagmi/viem; new —
  today only Sui + Phantom message-signing exist), per-chain status
  (pending → ACK'd, payout queue position).
- Curator dashboard: per-chain balances (free/reserved/equity/in-flight),
  bridge action (CCTP), HyperCore order panel, ConfigSync state.

## 10. Build order

Each phase lands green before the next starts; testnets: Sui testnet
(existing staging), Robinhood testnet, HyperEVM testnet.

1. **Message schema + golden fixtures** (Rust crate + Move + Solidity
   codecs). Verify: cross-codec fixture tests.
2. **Hub Move** — `multichain` module, `EndpointRegistry`,
   `endpoint-attestor`, ledger, appraisal spoke legs, payables/in-flight,
   handlers, ConfigSync events. Verify: `sui move test` suite incl.
   replay/staleness/refund-deadline cases.
3. **`SpokeVault` + `AttestorEndpoint`** in `evm-contracts/`; deposits,
   reclaim, withdraw queue, gates. Verify: Foundry tests. Deploy Robinhood
   testnet.
4. **`vault-messenger`** + StateSync + crank integration. Verify: e2e on
   Robinhood testnet — deposit → ACK → tradable; withdraw → ACK → paid;
   kill-relayer chaos (deposit reclaim after timeout; spoke freeze on stale
   heartbeat).
5. **HyperEVM spoke + `HyperCoreIntegration`**. Verify: e2e —
   deposit → ACK → `toCore` → order on HyperCore testnet → equity in
   `StateSync` → NAV on hub moves; payout-queue gate blocks `toCore` while
   a withdrawal is owed.
6. **CCTP integrations** (hub `bridge_out`/receive sweep, spoke
   `CCTPIntegration`, cctp-relay v2/EVM). Verify: hub↔HyperEVM round trip
   with in-flight accounting checked at each step (requires Sui CCTP v2 on
   testnet — re-verify availability first).
7. **`WormholeEndpoint`** for the Sui↔HyperEVM lane (second transport
   proving the abstraction).
8. **Frontend + deployment registration + staging rollout**; runbook
   (attestor key rotation, spoke freeze/unfreeze, stuck-message recovery).

## 11. Open questions / deferred

- **Robinhood USDC flavor** — native vs bridged; blocks final spoke config.
- **Whitelist policy for spoke depositors** — hub `deposit` enforces the
  `Whitelist`; spoke depositors have no Sui address. Proposal: per-spoke
  allowlist on `SpokeVault` (empty = open), hub skips its whitelist for
  ledger deposits. Needs a product decision.
- **Attestor key ops** — single key vs k-of-n at launch; custody/rotation.
- **Untranched-only** for multichain MVP; Senior/Junior spokes later (the
  ledger already carries basis; it needs a tranche code + lane interplay).
- **Spoke share transferability** (ERC-20) — needs hub round-trip or
  hub-authoritative balance proofs; deferred.
- **Cross-queue fairness** (hub lanes vs spoke queues under stress) and a
  permissionless force-repatriation backstop (spoke analog of
  `begin_force_session`) — deferred past MVP, documented risk: MVP relies
  on the payout-queue trading freeze to compel the curator.
- **Terminal close/settlement**: MVP forbids `initiate_close` while any
  spoke has non-zero assets/payables; spokes must be drained and unbound
  first.
- **In-flight CCTP loss/timeout handling** (attestation never arrives) —
  ops runbook item; funds are burn-and-mint so recovery is retrying the
  mint, but NAV holds the in-flight leg until receipt.
