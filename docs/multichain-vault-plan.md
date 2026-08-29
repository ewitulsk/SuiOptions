# Multichain Trading Vault: Sui Hub / EVM Spokes

Status: DESIGN — reviewed with Evan 2026-08-28/29 over three rounds; all
decisions below are locked. Not yet implemented.

## Summary

Extend trading-vault-v2 to a hub-and-spoke multichain vault. MVP scope:

- **Sui is the hub.** The existing `TradingVault` remains the single source
  of truth for ALL accounting: share supply, NAV, valuations, entry/exit
  haircuts, lockups, performance fees, tranche waterfall, withdrawal
  ordering. Nothing changes for single-chain vaults.
- **One spoke: Robinhood Chain.** Users deposit and withdraw **USDG** on the
  spoke; shares are issued by the hub. The spoke is a deliberately **dumb
  vault**: it custodies tokens, reports raw facts, and executes hub
  instructions. **A spoke never values anything and never computes shares
  or NAV** — it reports quantities ("user X deposited N of token T", "free
  balance is F"), and the hub does every conversion. Every message in §2.1
  obeys this.
- **No funds move between hub and spoke in MVP.** All vault-level bridging
  is cut. Spoke deposits stay on the spoke; hub assets stay on the hub;
  both count in one global NAV.
- **Curator controls funds on both sides.** On the hub, the existing
  session/adapter machinery is unchanged. On the spoke, the vault ships a
  **curator-controlled integration interface** (§6) — the only way funds
  can ever leave the spoke vault besides user payouts. MVP registers zero
  integrations; **a rebalance integration (so the curator can move funds
  and fill withdrawal queues) is a hard launch gate for mainnet** — MVP
  ships to testnet without it, mainnet does not.
- **Messaging is transport-agnostic**, and the protocol ships TWO real
  transports: **LayerZero V2** and **Chainlink CCIP** endpoint modules —
  both serve the Sui↔Robinhood lane end-to-end (§2.2). One endpoint is
  active per spoke at a time, admin-switchable. A relayer-gated endpoint
  is retained for local dev/test only, never production.

### Locked decisions (2026-08-28/29 reviews)

1. Messaging: agnostic endpoint interface with **both LayerZero and CCIP
   endpoint modules in the protocol** (§2.2); one active endpoint per
   spoke, switchable; relayer-gated endpoint for dev/test only.
2. Accounting: hub is master; spokes report raw quantities only — no
   valuation, NAV, or share math on any spoke, ever.
3. Withdrawals: hub-directed and share-denominated. The spoke notifies the
   hub the moment a request lands; the hub burns in full at ACK; the spoke
   **queues payouts it cannot fill** (FIFO) rather than the hub rationing
   ACKs. **Queued-but-unpaid payouts are tracked on the hub as `payables`
   and NAV is computed net of them** (§5.1). Accepted because the
   rebalance integration exists before mainnet (launch gate above).
4. Spoke depositor gating: a separate, per-chain whitelist on each
   `SpokeVault` (the hub's Sui `Whitelist` is not consulted).
5. Robinhood spoke deposit asset is **USDG**; testnet uses a
   faucet-mintable **mock USDG**, modeled on the `test-tokens` Sui mocks.
6. USDG valuation must be servable by **both** the Pyth and Switchboard
   oracle adapters, switchable at will via the existing `OracleRegistry`
   pin.
7. **Tranching is supported on spoke vaults**; all tranche accounting stays
   on the hub — the spoke relays the user's tranche choice as an opaque
   code.
8. Governance: composable role-based admin with transferable ownership on
   both sides (§6.1) — Sui via capability objects, EVM via two-step
   transferable roles.
9. Testnet-first; every network-dependent value in config profiles so
   mainnet is a config flip (§9).
10. 2026-08-29 descope: Hyperliquid (spoke, HyperCore/CoreWriter, Wormhole
    lane) cut entirely; vault-level bridge integrations (CCTP, Across) cut.
    Both return later as spoke integrations / endpoint modules.

### External facts this design depends on (verified 2026-08-29)

- Robinhood Chain: EVM (Arbitrum Orbit), mainnet 2026-07-01, testnet since
  2026-02-10. Per docs.robinhood.com/chain/bridging, **LayerZero
  (OFT/Stargate)** and **Chainlink CCIP/Transporter** (token transfers +
  arbitrary messaging) are live there — correcting this doc's earlier
  wrong claim that no third-party messaging served Robinhood — alongside
  the canonical Arbitrum bridge, Relay, Across, and LiFi/0x.
- **LayerZero V2 is live on Sui** (Move OApp packages: `register_oapp`,
  per-pathway `MessagingChannel` shared objects) and **CCIP supports
  Sui**, so BOTH stacks cover the Sui↔Robinhood messaging lane
  end-to-end.
- **USDG** (Paxos Global Dollar) is live on Robinhood Chain **as a
  LayerZero OFT** — the natural rail for the future rebalance
  integration. USDG does not exist on Sui today, so that integration must
  still handle asset transformation for any hub leg.
- LayerZero/CCIP availability on **Robinhood testnet** is unconfirmed —
  check at phase-3 start; the dev relayer endpoint (§2.2) covers local/CI
  either way.
- A **Pyth USDG/USD feed is unconfirmed** in the public catalog — check at
  phase-2 start. **Switchboard on-demand allows defining a USDG/USD feed
  ourselves**, so the Switchboard adapter path is guaranteed; Pyth is added
  the moment its catalog carries the pair (decision 6 is about the
  abstraction supporting both, which the existing adapter/pin design
  already does).

## 1. Roles and fund states

Hub: existing `vault_v2::TradingVault`, extended with a spoke ledger (§3).
Untranched and Senior/Junior vaults both supported.

Spoke vault fund states (tracked per asset; MVP: USDG only):

| State      | Meaning                                            | Curator can touch? |
|------------|----------------------------------------------------|--------------------|
| `pending`  | deposit escrowed, no hub ACK yet                   | NO                 |
| `active`   | hub-ACK'd; part of vault NAV                       | via integrations¹  |
| `reserved` | owed to a hub-ACK'd withdrawal (payout queue, §5)  | NO                 |

¹ MVP registers no integrations; `active` funds sit in the vault and back
withdrawals until the rebalance integration ships.

The pending→active transition happens ONLY on a hub `DepositAck` — the
"funds unusable until ACK" invariant, enforced on-chain on the spoke.

**Spoke depositor whitelist**: each `SpokeVault` carries its own allowlist
(role-managed, §6.1); `deposit` and `requestWithdraw` check it. Empty =
open. The hub does not re-check identity for spoke ledger entries.

**Hub valuation of spoke assets**: the hub's accounting asset stays USDC.
USDG is valued through the existing `PriceAttestation` machinery — a
USDG/USD feed servable by BOTH `oracle-switchboard` and `oracle-pyth`
adapters, with the active source chosen by the `OracleRegistry` pin
(`pin_oracle`/`unpin_oracle` switch it at any time, no vault changes). No
1:1 assumption.

## 2. Messaging layer

### 2.1 Envelope

Every message: `{src_chain_id, dst_chain_id, src_app, dst_app, seq, payload}`.
`seq` is per (src, dst, app-pair) and strictly increasing; receivers keep
`last_seq` and reject replays/out-of-order delivery (the relayer retries in
order until landed).

Payloads (bcs on Sui / abi-encoded on EVM; one canonical byte layout in the
schema crate, §8). Spoke→hub payloads are raw quantities; hub→spoke
payloads are instructions.

Spoke → Hub (facts):
- `DepositNotice { spoke_id, deposit_seq, depositor: bytes32, asset: u8,
  amount, tranche: u8 }` — asset is a spoke-local code bound in
  `SpokeConfig`; tranche is the user's choice relayed opaquely.
- `WithdrawRequest { spoke_id, request_seq, user: bytes32, tranche: u8,
  shares, all: bool }` — sent the moment the request lands on the spoke.
- `PayoutReceipt { spoke_id, request_seq, amount }`
- `StateSync { spoke_id, per-asset {free, reserved}, integration_raw, ts }`
  — `integration_raw` is raw venue data from any future integration (empty
  in MVP), never a computed value; the hub turns raw data into value.

Hub → Spoke (instructions):
- `DepositAck { deposit_seq, accepted: bool, shares }` — `shares` is
  hub-computed; the spoke records it verbatim for its UX mirror.
- `WithdrawAck { request_seq, user: bytes32, pay_amount }`
- `ConfigSync { paused, risk_off, curator: address, integrations_root }`

### 2.2 Transports and trust model

Both LayerZero V2 and Chainlink CCIP serve the Sui↔Robinhood lane
end-to-end, so hub↔spoke messages ride third-party verification networks
(LayerZero's configurable DVN set per pathway; CCIP's Chainlink DON) —
our own infrastructure initiates and pays for messages but is never the
thing the hub trusts. The protocol ships BOTH transports as endpoint
modules behind the same interface; **exactly one endpoint is active per
spoke at a time** (accepting two verifiers simultaneously would mean
either one can forge — switchability is redundancy, dual-acceptance is
double attack surface). Switching is an admin action on the hub,
propagated via `ConfigSync` through the currently-active endpoint.

Hub-side structure: each transport is a witness-typed Move module
allow-listed in a new `EndpointRegistry` (mirroring the oracle/integration
adapter pattern). A transport verifies delivery through its own stack and
constructs a `VerifiedMessage` hot potato that only `vault_v2::multichain`
can consume; `multichain` checks the spoke binding (`spoke_id →
(chain_id, spoke_vault_address, endpoint_type)`) and `seq` (our own
ordering/replay layer on top of any transport), then applies the payload.
Outbound: handlers hand an `OutboundMessage` to the active endpoint module
in the same PTB (LayerZero/CCIP sends are on-chain calls with fees).

- `endpoint-layerzero`: Sui Move OApp (`register_oapp`, per-pathway
  `MessagingChannel`); verifies via the lane's configured DVN set.
- `endpoint-ccip`: Sui CCIP client; verifies via the Chainlink DON.
- `endpoint-relayer` (dev/test ONLY, never bound in production):
  registered-sender gate for localnet/CI where neither stack exists.

Message fees: LayerZero/CCIP charge per message (native token / LINK).
Spoke→hub messages sent inside user transactions (deposit, withdraw
request) need fee funding — either `msg.value` from the user or a
vault-held fee pot topped up by ops; hub→spoke sends are paid by the crank
service. Open item §11.

### 2.3 Spoke side (EVM)

`SpokeVault` talks to an `IMessageEndpoint`:

```solidity
interface IMessageEndpoint {
    function send(bytes calldata envelopeAndPayload) external;   // outbound
    // inbound: endpoint validates delivery, then calls
    // spokeVault.handleMessage(envelope, payload); vault checks
    // msg.sender == registered endpoint + seq.
}
```

- `LayerZeroEndpoint.sol`: OApp wired to Robinhood's LayerZero endpoint;
  peers pinned to the hub OApp.
- `CCIPEndpoint.sol`: CCIP sender/receiver wired to Robinhood's
  router; source chain/sender pinned to the hub.
- `RelayerEndpoint.sol` (dev/test only): `msg.sender` must hold
  `RELAYER_ROLE` (§6.1).

Endpoint per spoke set at deploy, changeable only via hub `ConfigSync`.

## 3. Hub accounting extensions (`vault_v2::multichain`)

New module(s) in the `trading-vault-v2` package (upgrade), keeping
`vault.move` changes minimal and behind `public(package)` helpers.

- **Spoke registry**: shared `MultichainRegistry` (AdminCap-gated):
  `spoke_id → SpokeConfig { chain_id, vault_address: bytes32, endpoint:
  TypeName, asset_codes: asset code → Sui-side TypeName for valuation,
  active }`; per-vault binding via `CuratorCap` + admin co-sign (matches
  the `set_external_account` posture).
- **Spoke holdings ledger** (per vault, dynamic field):
  `Table<SpokeUserKey{spoke_id, user: bytes32}, Holding { tranche: Tranche,
  shares: u128, basis: u64, deposited_at_ms: u64 }>` — same share math
  (`SHARE_OFFSET`, floor division), same per-tranche book: **spoke shares
  mint into the same `TrancheBook` supplies as hub positions**, so the
  waterfall, hurdle accrual, junior generations, and NAV/share are global
  across chains with zero spoke-side logic. (One holding per (spoke, user,
  tranche).)
- **Per-spoke asset accounting** on the vault:
  `SpokeAssets { per-asset {free, reserved}, integration_value,
  last_sync_ms, payables }`. Raw balances from `StateSync`;
  `integration_value` is 0 in MVP (populated by future integrations'
  hub-side valuation adapters).
  - **Appraisal extension**: `begin_appraisal` snapshots bound spokes into
    the `Appraisal` hot potato (like `external_pending`); a
    `record_spoke_state` leg per spoke (bound endpoint only,
    freshness-checked) values per-asset balances at attested prices (USDG
    via its pinned feed) plus `integration_value`, minus `payables`. NAV
    cannot complete with a stale spoke → the safe failure mode.
- **Message handlers** (consume `VerifiedMessage`; valuation happens here,
  against a complete `Appraisal`):
  - `handle_deposit_notice`: vault open + not paused + tranche valid →
    value `amount` at the attested price, apply entry haircut, mint shares
    into the requested tranche at current NAV, record Holding, bump spoke
    `free` → `DepositAck{accepted: true, shares}`. Rejections →
    `accepted: false` (spoke refunds).
  - `handle_withdraw_request`: validate holding + lockup + the same
    per-tranche gates hub withdrawals face (junior blocked in risk-off
    states, generation wipe checks), value shares **in full** at current
    NAV with exit haircut, crystallize perf fee vs basis, burn the shares,
    `payables += pay_amount` → `WithdrawAck`. Rejected → `pay_amount: 0`.
  - `handle_payout_receipt`: clear payable, deduct spoke balance.
- **Gate propagation**: `ConfigSync` pushed on pause, risk-state flips from
  `sync_capital`, curator rotation, integration-set changes. Spokes treat a
  stale heartbeat (> N min) as `risk_off = true` locally — integrations
  freeze; deposits and payouts continue.

## 4. Deposit flow (spoke)

1. User (spoke-whitelisted): `SpokeVault.deposit(asset, amount, tranche)` →
   escrowed `pending`, local `deposit_seq`, `DepositNotice` sent
   immediately. The spoke validates nothing about the tranche beyond
   range — the hub owns policy.
2. Hub handler mints ledger shares (entry haircut, basis, hub-clock
   timestamp) → `DepositAck`.
3. Spoke on ACK: `pending → active`; records the hub-stated share count in
   its non-authoritative per-tranche mirror (UX only).
4. Refund path: no ACK within `deposit_timeout` (e.g. 24h) → depositor may
   `reclaim(deposit_seq)`. The hub refuses to ACK a notice older than
   `deposit_timeout − margin` (chain-timestamped), so a late ACK for a
   reclaimed deposit cannot occur; an ACK for a reclaimed seq is rejected
   by the spoke and alarmed.

Spoke shares are non-transferable claims in MVP (no ERC-20).

## 5. Withdrawal flow (spoke) — hub-directed, spoke-queued

The hub is master; the spoke asks the moment a request lands and waits for
the answer. Requests are **share-denominated** (`shares` or `all`, per
tranche); dollar estimates are a UI concern served from hub NAV via the
API — the spoke computes nothing.

1. User: `requestWithdraw(tranche, shares | all)` → spoke records the
   request (one in-flight per (user, tranche)) and sends `WithdrawRequest`
   in the same transaction.
2. Hub: validates against the ledger, enforces lockup + tranche gates,
   prices the full request at current NAV with exit haircut, crystallizes
   fees, **burns the shares in full**, books `payables` →
   `WithdrawAck{user, pay_amount}` denominated in the spoke's deposit
   asset at attested prices.
3. Spoke on ACK: pay immediately from `free` if it can; otherwise move
   what exists to `reserved` and **queue the remainder FIFO**. While any
   payout is queued, integration calls that move funds out revert, and all
   funds arriving service the queue before becoming `active` — so once the
   rebalance integration exists, refilling the queue is a curator
   obligation enforced by the trading freeze.
4. On each payment: `PayoutReceipt` → hub clears the payable.

### 5.1 Hub-tracked payables: NAV net of the queue

Queued-but-unpaid withdrawals are tracked **on the hub** and NAV is
computed against them. At ACK time the hub burns the shares and books the
owed amount into `SpokeAssets.payables` (denominated in the spoke's
deposit asset). Every appraisal then values the vault as:

```
NAV = hub assets + Σ_spokes (free + reserved + integration_value)·px
      − Σ_spokes payables·px
```

with `px` the attested price of each spoke asset — so a payable is a
liability marked at current prices until the spoke's `PayoutReceipt`
clears it (which simultaneously removes the liability and the reserved
balance backing it, net zero). Properties this buys:

- Remaining holders are unaffected by queue duration: the exiting user's
  claim converted from shares to a fixed asset-amount at ACK, exactly like
  a hub-side `fulfill_next` payout that just hasn't physically settled.
- The hub always knows every spoke's queue (its own `payables` book), so
  queue depth/age feed the curator dashboard and ops alerts from hub
  state, not spoke scraping.
- A `StateSync` cross-check (`reserved` on the spoke vs `payables` on the
  hub) catches divergence and alarms.

Accepted consequence (2026-08-29): in MVP-on-testnet the queue can only be
filled by new deposits; on mainnet the rebalance integration (launch gate)
gives the curator the means — and the freeze the obligation — to fill it.

## 6. Curator integration interface (spoke)

The only path for funds to leave a `SpokeVault` other than user payouts.
Ships in MVP as an interface with **zero registered integrations**; the
first real one (curator rebalance) is the mainnet launch gate.

```solidity
interface ISpokeIntegration {
    // funds flow vault -> integration only via SpokeVault.extendTo:
    // curator-only, active-funds-only, blocked while any payout is queued
    // and while risk_off/stale-heartbeat; integration must be registered.
    function onFundsReceived(address asset, uint256 amount) external;
    // raw state for StateSync.integration_raw (never a valuation)
    function rawState() external view returns (bytes memory);
    // permissionless return path: anyone can push funds back to the vault
}
```

- **Registration is hub-governed**: the integration set for a spoke is
  committed on the hub (AdminCap + CuratorCap co-signed, like adapter
  allow-listing) and propagated via `ConfigSync.integrations_root`; the
  spoke accepts only registered integrations, and no spoke-local role can
  add one. Removal is an instant kill switch for new `extendTo` calls,
  mirroring `IntegrationRegistry` semantics on the hub.
- Every integration must expose `rawState()`; a hub-side valuation adapter
  (oracle-registry slot) is part of registering any integration.
- Curator identity on the spoke comes from `ConfigSync` (rotation
  propagates from the hub; the curator is NOT a spoke-local role).

### 6.1 Governance and admin ownership

Composable roles with transferable ownership on both sides; policy-bearing
decisions live on the hub, operational knobs live where they act.

Hub (Sui): capability objects, already the repo's pattern and transferable
by construction — `AdminCap` (protocol admin: `MultichainRegistry`,
`EndpointRegistry` relayer set, integration registration co-sign) and
`CuratorCap` (per-vault binding + integration co-sign, rotatable via the
existing `rotate_curator_by_curator` machinery). Transferring hub admin =
transferring the `AdminCap` object; nothing else to migrate.

EVM (`SpokeVault` + `RelayerEndpoint`): OpenZeppelin
`AccessControlDefaultAdminRules` — one `DEFAULT_ADMIN_ROLE` root with
built-in **two-step, time-delayed transfer** (deployer EOA at first, handed
to a multisig later with no code change), administering three operational
roles:
- `WHITELIST_ROLE` — manage the spoke depositor allowlist.
- `RELAYER_ROLE` — accounts allowed to deliver messages (on the endpoint);
  multiple accounts for redundancy, rotated without redeploys.
- `PAUSER_ROLE` — local emergency pause (deposits/payouts halt; hub
  `ConfigSync` pause remains the governed path — this is the break-glass).

Explicitly NOT spoke-local roles: curator identity and the integration set
(both hub-propagated via `ConfigSync`), so spoke admin compromise cannot
redirect funds — worst case it can pause, censor the whitelist, or stall
relaying, all liveness not custody.

## 7. Spoke contracts (new `evm-contracts/` Foundry workspace)

- `SpokeVault.sol` — per-asset fund states + depositor whitelist +
  deposit/reclaim + per-tranche withdraw + payout queue + `handleMessage`
  dispatch + endpoint binding + integration registry (§6) + roles (§6.1) +
  pause/stale-heartbeat freeze.
- `endpoints/LayerZeroEndpoint.sol`, `endpoints/CCIPEndpoint.sol`,
  `endpoints/RelayerEndpoint.sol` (dev/test only) — all implementing
  `IMessageEndpoint` (§2.3).
- `TUSDG.sol` — faucet-mintable mock USDG for testnet (open `mint`, 6
  decimals), mirroring the `test-tokens` Sui pattern (TUSDC et al.);
  deployed only in the testnet config set.
- Shared message codec library mirroring the Move schema; golden byte
  fixtures keep Move/Solidity/Rust codecs in lockstep.

## 8. Off-chain

- **New `rust-backend/services/vault-messenger`** (patterns: `cctp-relay`
  watcher + existing service key handling): with LayerZero/CCIP carrying
  verification, this service is an **initiator and fee payer, not a trust
  gate** — it triggers hub-side sends (ACKs/ConfigSync PTBs through the
  active endpoint, paying message fees), calls the spoke's permissionless
  `syncState()` on an interval, watches both chains to confirm delivery,
  and retries/escalates stuck messages (both stacks have retry/exec
  semantics); per-chain service accounts in AWS Secrets;
  `error!(alert_id = "tx-failed-vault-messenger")` per the tx-alerting
  convention; alerts on payout-queue age and undelivered-message age.
  Full 9-spot deployment registration per repo convention.
- Appraisal/crank flow gains the spoke legs: price attestations (incl.
  USDG/USD) + `record_spoke_state` per spoke + existing legs.
- Oracle service: USDG/USD feed served through the **Switchboard** adapter
  (on-demand feed we define — guaranteed available) and through the
  **Pyth** adapter as soon as its catalog carries USDG/USD; the
  `OracleRegistry` pin selects the active one.

## 9. Config: testnet → mainnet is a flip

All network-dependent values live in per-network config profiles, never in
code — the repo's existing pattern (`config.staging.toml`/`config.prod.toml`
with a `network` profile, as cctp-relay does; `rust-backend/deployments.json`
for published IDs; frontend per-network maps in `config.ts`):
- Per-chain: RPC URLs, chain IDs, spoke vault + endpoint addresses, asset
  addresses (USDG mainnet / TUSDG testnet), relayer accounts.
- Hub: package/registry object IDs per Sui network.
- One `network_set` selector (testnet-set vs mainnet-set) chooses the whole
  coherent bundle; promotion is a config/deploy change with zero code
  edits.

## 10. Build order

1. **Message schema + golden fixtures** (Rust crate + Move + Solidity
   codecs). Verify: cross-codec fixture tests.
2. **Hub Move** — `multichain` module, `EndpointRegistry` + the dev
   `endpoint-relayer`, tranche-aware ledger, appraisal spoke legs,
   payables, handlers, ConfigSync events, USDG oracle pin (Switchboard
   feed; Pyth catalog check happens here). Verify: `sui move test` incl.
   replay/staleness/refund-deadline/tranche-gate/payables-NAV cases.
3. **`SpokeVault` + dev `RelayerEndpoint` + `TUSDG`** in `evm-contracts/`;
   roles, whitelist, deposits, reclaim, per-tranche withdraw + payout
   queue, integration registry (empty), gates. Verify: Foundry tests.
   Deploy Robinhood testnet.
4. **`vault-messenger`** + StateSync + crank integration, over the dev
   endpoint. Verify: e2e on Robinhood testnet — TUSDG deposit → ACK →
   active; withdraw → ACK → paid → receipt clears payable; underfunded
   withdraw queues FIFO and drains from a fresh deposit; chaos (reclaim
   after timeout; freeze on stale heartbeat).
5. **LayerZero endpoints** — `endpoint-layerzero` (Sui OApp) +
   `LayerZeroEndpoint.sol`; bind the testnet spoke to it (or stay on the
   dev endpoint if LayerZero is absent on Robinhood testnet, and verify
   on a supported testnet lane). Verify: the phase-4 e2e suite re-run over
   LayerZero, plus an endpoint-switch drill (dev → LZ via ConfigSync).
6. **CCIP endpoints** — `endpoint-ccip` (Sui client) + `CCIPEndpoint.sol`;
   same verification + an LZ → CCIP switch drill. Both transports must
   pass before mainnet.
7. **Frontend + deployment registration + staging rollout**; runbook
   (endpoint switching, spoke freeze/unfreeze, stuck-message recovery,
   fee-pot top-up, payout-queue-age alert response).

Post-MVP, pre-mainnet (launch gate): the curator rebalance integration
(§6) with its hub-side valuation adapter and in-flight accounting.
Later: Hyperliquid spoke + HyperCore/CoreWriter; bridge integrations;
third-party endpoint modules (Wormhole/LayerZero); ERC-20 spoke shares.

## 11. Open items

- **Pyth USDG/USD catalog check** (phase 2) — Switchboard path is
  guaranteed either way; this only decides when the second oracle lights
  up.
- **Message-fee funding** (§2.2) — who pays LayerZero/CCIP fees on
  user-initiated spoke messages: `msg.value` from the user vs a vault-held
  fee pot topped up by ops. Recommendation: fee pot (cleaner UX, bounded
  cost); needs Evan's confirmation.
- **Primary transport per lane** — recommendation: LayerZero primary
  (mature Sui Move OApp tooling; USDG already rides LayerZero on
  Robinhood), CCIP as the tested standby; confirm before phase 5.
- **LayerZero/CCIP availability on Robinhood testnet** — check at phase 5;
  dev endpoint covers earlier phases regardless.
- **Service account ops** — gas/LINK funding and rotation runbook for the
  messenger's fee-paying accounts (ordinary service accounts, not trust
  roots).
- **Rebalance-integration design** (the launch gate) — deliberately not
  designed here; the USDG LayerZero OFT is the obvious rail for its
  spoke legs; it will need destination restrictions to registered vault
  addresses, in-flight accounting, and a hub-side valuation adapter.
- Accepted MVP limitations (signed off 2026-08-29): spoke funds are inert
  until the rebalance integration ships; on testnet, payout queues can
  only drain from new deposits; terminal close blocked while a spoke holds
  assets/payables; dark spoke blocks vault-wide NAV (safe direction);
  messaging trust rides the active transport's verifier network (LayerZero
  DVN set / Chainlink DON) plus our per-lane security configuration.
