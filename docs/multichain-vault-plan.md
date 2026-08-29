# Multichain Trading Vault: Sui Hub / EVM Spokes

Status: DESIGN — reviewed with Evan 2026-08-28 and 2026-08-29; decisions below
are locked EXCEPT the Robinhood bridge selection (§6.3). Not yet implemented.

## Summary

Extend trading-vault-v2 to a hub-and-spoke multichain vault:

- **Sui is the hub.** The existing `TradingVault` remains the single source of
  truth for ALL accounting: share supply, NAV, valuations, entry/exit
  haircuts, lockups, performance fees, tranche waterfall, withdrawal
  ordering. Nothing about hub-side deposits, sessions, or the capital
  machinery changes for single-chain vaults.
- **Every other chain is a spoke.** A spoke is a deliberately **dumb vault**:
  it custodies tokens, accepts deposits and withdrawal requests, reports raw
  facts to the hub, and executes hub instructions. **A spoke never values
  anything and never computes shares or NAV** — it reports quantities ("user
  X deposited N of token T", "free balance is F", "HyperCore equity reads
  E"), and the hub does every conversion to value and shares. Every message
  in §2.1 obeys this: spoke→hub payloads carry raw quantities only;
  hub→spoke payloads carry instructions (mint record / pay amounts) that the
  spoke executes verbatim.
- **Messaging is transport-agnostic.** Hub and spoke each speak to an
  endpoint abstraction; concrete transports (our own attestor, Wormhole,
  LayerZero) plug in per spoke.
- **Bridging is a curator-executed vault integration, not messaging.**
  Bridge adapters (CCTP, Across, …) plug in per lane behind one interface;
  the only universal restriction is that funds may move **only between
  registered vault addresses** (hub vault or spoke vaults). **No amount
  limits** — a market-maker rebalance must never be caught by a budget cap.
- **The curator holds keys on every supported chain** and interacts with a
  spoke the same way they do the hub: integrations only, never withdrawal.

MVP target: Sui hub + **Robinhood Chain spoke (USDG deposits/withdrawals
only)** + **Hyperliquid spoke (USDC deposits/withdrawals + vault-custodied
HyperCore trading via CoreWriter)**.

### Locked decisions (2026-08-28 / 2026-08-29 reviews)

1. Messaging: agnostic endpoint interface; integrate whatever concrete
   transports are necessary to cover Robinhood and Hyperliquid.
2. Accounting: hub is master; spokes are dumb executors reporting raw
   quantities. NO valuation, NAV, or share math on any spoke, ever.
3. Withdrawals in scope, hub-directed (spoke asks, waits, hub answers "pay
   user X amount A"). Requests are share-denominated.
4. Hyperliquid trading is vault-custodied via CoreWriter + read precompiles.
5. Spoke depositor gating: a **separate, per-chain whitelist on each
   `SpokeVault`** (the hub's Sui `Whitelist` is not consulted for spoke
   deposits).
6. Robinhood spoke deposit asset is **USDG** (no liquid USDC there; Across
   delivers inbound USDC as USDG on Robinhood). Hyperliquid spoke uses
   native USDC.
7. **Tranching is supported on spoke vaults**, but all tranche accounting
   (waterfall, hurdle, lanes/gates, generations) stays on the hub — the
   spoke only relays the user's tranche choice as an opaque code.
8. **No bridge amount limits.** Destination restriction (registered vault
   addresses only) is the entire control.
9. Testnet-first, but every network-dependent value lives in config
   profiles so mainnet is a config flip, not a code change (§9).
10. OPEN: which bridge serves the Robinhood lanes (§6.3). Across is the
    candidate; Evan is validating. CCTP remains the hub↔Hyperliquid lane.

### External facts this design depends on (verified 2026-08-29)

- Robinhood Chain: Arbitrum Orbit L2, mainnet 2026-07-01. No
  Wormhole/LayerZero/CCTP → our attestor transport is required there.
  Chainlink is live on it (price feeds available if ever needed EVM-side —
  we don't need them; valuation is hub-side).
- **USDG chains**: Ethereum, Solana, Ink, X Layer, **Robinhood Chain**;
  USDG0 (LayerZero OFT variant) on Hyperliquid, Plume, Aptos. **USDG does
  not exist on Sui** — bridged-out Robinhood funds cannot land on the hub
  as USDG.
- **Across**: supports Robinhood Chain and HyperEVM (among ~13+ chains);
  inbound USDC arrives on Robinhood as USDG. **Across does not support
  Sui**, so there is no direct Robinhood↔hub Across lane.
- HyperEVM: native USDC + CCTP v2 live; Wormhole and LayerZero both live.
- Sui: CCTP **v1** only today; Circle targeted canonical v2 for ~end of
  H1 2026 and began v1 phase-out 2026-07-31. HyperEVM is v2-only, so the
  hub↔Hyperliquid CCTP lane is gated on Sui v2 — re-verify at phase 6.

## 1. Roles and fund states

Hub: existing `vault_v2::TradingVault`, extended with a spoke ledger (§3).
Untranched and Senior/Junior vaults both supported.

Per-spoke deposit asset config (part of `SpokeConfig`, §3):

| Spoke      | Deposit asset | Curator trading integrations        |
|------------|---------------|-------------------------------------|
| Robinhood  | USDG          | none (deposits/withdrawals + bridge)|
| Hyperliquid| USDC (native) | HyperCore via CoreWriter            |

Spoke vault fund states (tracked per asset):

| State      | Meaning                                            | Curator can touch? |
|------------|----------------------------------------------------|--------------------|
| `pending`  | deposit escrowed, no hub ACK yet                   | NO                 |
| `active`   | hub-ACK'd; part of vault NAV                       | via integrations   |
| `reserved` | owed to a hub-ACK'd withdrawal (payout queue, §5)  | NO                 |

The pending→active transition happens ONLY on a hub `DepositAck` — the
"funds unusable until ACK" invariant, enforced on-chain on the spoke.

**Spoke depositor whitelist**: each `SpokeVault` carries its own allowlist
(admin-managed, per decision 5); `deposit` and `requestWithdraw` check it.
Empty list = open, per-spoke choice. The hub does not re-check identity for
spoke ledger entries.

**Hub valuation of spoke assets**: the hub's accounting asset stays USDC.
USDG is valued on the hub through the existing `PriceAttestation` machinery —
a USDG/USD feed added to the oracle service and pinned in `OracleRegistry`,
exactly like any other non-accounting deposit asset. No 1:1 assumption.

## 2. Messaging layer

### 2.1 Envelope

Every message: `{src_chain_id, dst_chain_id, src_app, dst_app, seq, payload}`.
`seq` is per (src, dst, app-pair) and strictly increasing; receivers keep
`last_seq` and reject replays/out-of-order delivery (the relayer retries
in order until landed).

Payloads (bcs on Sui / abi-encoded on EVM; one canonical byte layout defined
in the schema crate, §8). Spoke→hub payloads are raw quantities; hub→spoke
payloads are instructions.

Spoke → Hub (facts):
- `DepositNotice { spoke_id, deposit_seq, depositor: bytes32, asset: u8,
  amount, tranche: u8 }` — asset is a spoke-local asset code bound in
  `SpokeConfig`; tranche is the user's choice relayed opaquely.
- `WithdrawRequest { spoke_id, request_seq, user: bytes32, tranche: u8,
  shares, all: bool }`
- `PayoutReceipt { spoke_id, request_seq, amount }`
- `BridgeReceipt { spoke_id, transfer_id, asset: u8, amount }` — bridged
  funds arrived at this spoke.
- `StateSync { spoke_id, per-asset {free, reserved}, integration_raw, ts }`
  — `integration_raw` is raw venue data (e.g. HyperCore balances/positions
  /marks from the read precompiles), NOT a computed equity value; the hub's
  oracle leg turns it into value (§3).

Hub → Spoke (instructions):
- `DepositAck { deposit_seq, accepted: bool, shares }` — `shares` is a
  hub-computed number the spoke records verbatim for its UX mirror.
- `WithdrawAck { request_seq, user: bytes32, pay_amount }`
- `ConfigSync { paused, risk_off, curator: address, integrations_root }`

### 2.2 Why an attestor key (and hub-side transports)

The hub cannot observe Robinhood Chain (or any spoke). When a message
claims "user X deposited N", the hub needs on-chain-verifiable proof or
anyone could mint shares by calling the handler. On lanes served by
Wormhole/LayerZero that proof is the transport's validator signatures; no
transport serves Robinhood, so the proof must be OUR signature: the
attestor key. (Gating on the relayer's tx-sender address is the same trust
with less flexibility — a signed-message design verifies identically on Sui
and EVM, supports k-of-n, and rotates without changing gas payers. It also
matches the existing `registrar_pubkey` / hedge-signer patterns.)

Each transport is a witness-typed Move module allow-listed in a new
`EndpointRegistry` (mirroring the oracle/integration adapter pattern). A
transport verifies its own proof and constructs a `VerifiedMessage` hot
potato that only `vault_v2::multichain` can consume; `multichain` checks
the spoke binding (`spoke_id → (chain_id, spoke_vault_address,
endpoint_type)`) and `seq`, then applies the payload. Outbound: `multichain`
emits a canonical `OutboundMessage` event; transports needing an on-chain
send (Wormhole publish) wrap it, the attestor transport just relays it.

- `endpoint-attestor` (MVP; required for Robinhood): ed25519 signature(s)
  over `domain_tag ‖ envelope ‖ payload`, pubkeys in `EndpointRegistry`.
  Start with 1 key, structured for k-of-n. Key custody/rotation: ops item.
- `endpoint-wormhole` (phase 7): VAA verification, for the Sui↔HyperEVM
  lane — second transport proving the abstraction and removing our key from
  that lane's trust path.

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

Endpoint per spoke set at deploy, changeable only via hub `ConfigSync`.

## 3. Hub accounting extensions (`vault_v2::multichain`)

New module(s) in the `trading-vault-v2` package (upgrade), keeping
`vault.move` changes minimal and behind `public(package)` helpers.

- **Spoke registry**: shared `MultichainRegistry` (admin-gated):
  `spoke_id → SpokeConfig { chain_id, vault_address: bytes32, endpoint:
  TypeName, asset_codes: asset code → Sui-side TypeName for valuation,
  active }`; per-vault binding via `CuratorCap` + admin co-sign (matches
  the `set_external_account` posture).
- **Spoke holdings ledger** (per vault, dynamic field):
  `Table<SpokeUserKey{spoke_id, user: bytes32}, Holding { tranche: Tranche,
  shares: u128, basis: u64, deposited_at_ms: u64 }>` — same share math
  (`SHARE_OFFSET`, floor division), same per-tranche book: **spoke shares
  mint into the same `TrancheBook` supplies as hub positions**, so the
  §3.4a waterfall, hurdle accrual, junior generations, and NAV/share are
  global across chains with zero spoke-side logic. (One holding per
  (spoke, user, tranche); a second tranche deposit keys a second entry.)
- **Per-spoke asset accounting** on the vault:
  `SpokeAssets { per-asset {free, reserved}, integration_value,
  last_sync_ms, in_flight_out, payables }`.
  - Raw balances from `StateSync`; `integration_value` computed hub-side
    from `integration_raw` by the pinned oracle adapter (a HyperCore
    valuation adapter witness — the same trust slot as `PriceAttestation`
    sources).
  - **Appraisal extension**: `begin_appraisal` snapshots bound spokes into
    the `Appraisal` hot potato (like `external_pending`); a
    `record_spoke_state` leg per spoke (bound endpoint only,
    freshness-checked) values per-asset balances at attested prices (USDG
    via its feed) plus `integration_value` plus in-flight, minus payables.
    NAV cannot complete with a stale spoke → the safe failure mode.
- **Message handlers** (consume `VerifiedMessage`; valuation happens here,
  against a complete `Appraisal`):
  - `handle_deposit_notice`: vault open + not paused + tranche valid for
    the vault's capital structure → value `amount` of the mapped asset at
    attested price, apply entry haircut, mint shares into the requested
    tranche at current NAV, record Holding, bump spoke `free` →
    `DepositAck{accepted: true, shares}`. Rejections → `accepted: false`
    (spoke refunds).
  - `handle_withdraw_request`: validate holding + lockup + the same
    per-tranche gates hub withdrawals face (junior blocked in the §8.4b
    risk states, generation wipe checks), value shares at current NAV with
    exit haircut, crystallize perf fee vs basis (fee shares to the curator
    commitment position), **burn shares**, `payables += pay_amount` →
    `WithdrawAck`. Rejected → `pay_amount: 0`.
  - `handle_payout_receipt`: clear payable, deduct spoke balance.
  - `handle_bridge_receipt`: clear matching `in_flight_out` (§6).
- **Gate propagation**: `ConfigSync` pushed on pause, risk-state flips from
  `sync_capital`, curator rotation. Spokes treat a stale heartbeat (> N
  min) as `risk_off = true` locally — curator integrations freeze; deposits
  and payouts continue.

## 4. Deposit flow (spoke)

1. User (spoke-whitelisted): `SpokeVault.deposit(asset, amount, tranche)` →
   escrowed `pending`, local `deposit_seq`, `DepositNotice` sent. The spoke
   validates nothing about the tranche beyond range — the hub owns policy.
2. Hub handler mints ledger shares (entry haircut, basis, hub-clock
   timestamp) → `DepositAck`.
3. Spoke on ACK: `pending → active`; records the hub-stated share count in
   its non-authoritative per-tranche mirror (UX only). Only now is the
   money tradable.
4. Refund path: no ACK within `deposit_timeout` (e.g. 24h) → depositor may
   `reclaim(deposit_seq)`. The hub refuses to ACK a notice older than
   `deposit_timeout − margin` (chain-timestamped), so a late ACK for a
   reclaimed deposit cannot occur; an ACK for a reclaimed seq is rejected
   by the spoke and alarmed.

Spoke shares are non-transferable claims in MVP (no ERC-20; §11).

## 5. Withdrawal flow (spoke) — hub-directed

The hub is master; the spoke asks and waits. Requests are
**share-denominated** (`shares` or `all`, per tranche) — NAV moves between
request and ACK, so a $-request can misprice in transit. Dollar estimates
are a UI concern served from hub NAV via the API; the spoke computes
nothing.

1. User: `requestWithdraw(tranche, shares | all)` → spoke records the
   request (one in-flight per (user, tranche)), sends `WithdrawRequest`.
2. Hub: validates against the ledger, enforces lockup + tranche gates,
   prices at current NAV with exit haircut, crystallizes fees, burns
   shares, books `payables` → `WithdrawAck{user, pay_amount}` (denominated
   in the spoke's deposit asset, converted at attested prices).
3. Spoke on ACK: pay immediately if `free ≥ pay_amount`; else reserve what
   exists and queue the remainder (FIFO). **While the payout queue is
   non-empty, curator integration calls that move funds out revert**, and
   all funds arriving service the queue before becoming `active` — bridging
   money back is mandatory before trading resumes on that spoke.
4. On each payment: `PayoutReceipt` → hub clears the payable.

Hub-side FIFO lanes are untouched: hub lanes govern hub liquidity, spoke
queues govern spoke liquidity; the hub applies the same tranche-gate policy
to both. Cross-chain queue fairness is deferred (§11).

## 6. Curator bridging: the bridge-adapter integration

Bridging moves vault funds between chains. It is a curator action executed
through per-lane **bridge adapters** behind one interface, with exactly one
universal rule: **destination must be a registered vault address** (hub
vault or a bound spoke vault). **No amount limits** (decision 8) — a
rebalance must never be caught by a cap; risk-off gating still applies on
the source side (moving funds is a curator action like any other).

Every transfer is tracked so NAV never loses or double-counts in-flight
funds: source books `in_flight_out{transfer_id, asset, amount, dst}` at
send; destination emits `BridgeReceipt` on arrival; the hub moves the
amount from in-flight to the destination's balance (hub arrivals clear
directly). Asset transformation en route (e.g. USDC in → USDG out on
Robinhood via Across) is recorded on receipt — in-flight is valued as the
source asset until the receipt states what landed.

### 6.1 Hub adapter surface (Sui)

- Outbound: curator `Session`-style `bridge_out<T>` takes from free balance
  (risk-off gated, unlimited amount) and hands the funds to an allow-listed
  bridge-adapter witness (CCTP first: burn ticket with `mint_recipient` =
  spoke vault address); records in-flight.
- Inbound: mint/delivery to **the vault object's own address**, swept via
  the existing `receive_coin<T, W>`/`Receiving` path from a crank session,
  clearing in-flight.

### 6.2 Spoke adapter surface (EVM)

`IBridgeAdapter { bridgeTo(dst_vault_id, asset, amount) }` — curator-only,
active-funds-only, payout-queue-gated, recipient forced to the registered
address for `dst_vault_id`. Adapters:
- `CCTPAdapter` (HyperEVM): CCTP v2 `depositForBurn`; inbound completion is
  permissionless; vault credits on mint and emits `BridgeReceipt`.
- `AcrossAdapter` (Robinhood + HyperEVM, pending §6.3): deposit into
  Across's SpokePool with recipient = destination vault; inbound funds
  arrive by relayer fill at the vault, credited on receipt hook/sweep.

### 6.3 Lane map and the OPEN bridge decision

Confirmed lane facts: Across covers **Robinhood↔HyperEVM** (both live on
Across; USDC in → USDG on Robinhood) but **not Sui**; USDG does not exist
on Sui; CCTP covers **hub↔HyperEVM** once Sui v2 ships. So no single
bridge serves Robinhood↔hub directly.

Proposed resolution (pending Evan's bridge validation): **two-hop
rebalancing** — Robinhood ↔ Hyperliquid via Across, Hyperliquid ↔ hub via
CCTP. Every pair is reachable, each hop is a tracked vault-to-vault
transfer, and no new bridge trust is added. The direct Robinhood↔hub lane
simply doesn't exist in MVP. If a better bridge is found this slots in as
another adapter without design change. DECISION OPEN: Evan validating
whether Across's Robinhood lanes (assets, amounts, contract-initiated
deposits with fixed recipient) 100% fit; adapter work (phase 6) doesn't
start until locked.

## 7. Spoke contracts (new `evm-contracts/` Foundry workspace)

- `SpokeVault.sol` — per-asset fund states + depositor whitelist +
  deposit/reclaim + per-tranche withdraw request/queue + `handleMessage`
  dispatch + endpoint binding + curator address (from `ConfigSync`) +
  integration/adapter registry + pause/stale-heartbeat freeze.
- `AttestorEndpoint.sol`; later `WormholeEndpoint.sol`.
- `adapters/CCTPAdapter.sol`, `adapters/AcrossAdapter.sol` (§6).
- `integrations/HyperCoreIntegration.sol` (HyperEVM only): vault-custodied
  HyperCore account (its own contract address); curator-only, whitelisted
  CoreWriter actions: `toCore/fromCore` (USDC transfers strictly between
  the EVM vault and its HyperCore account), `placeOrder`/`cancelOrder`
  (perp + spot limit orders and cancels only — no withdraw-to-address, no
  agent approvals). `coreState()` returns **raw** balances/positions/marks
  from the read precompiles for `StateSync.integration_raw`; the hub's
  pinned adapter computes the value. CoreWriter actions are async (executed
  a block later); the attested `StateSync` timestamp covers the gap.
- Shared message codec library mirroring the Move schema; golden byte
  fixtures keep Move/Solidity/Rust codecs in lockstep.

## 8. Off-chain

- **New `rust-backend/services/vault-messenger`** (patterns: `cctp-relay`
  watcher + `hedge-signer` key handling): watches hub events and spoke
  logs, persists ordered per-lane message queues in Postgres,
  attestor-signs, submits with per-chain relayer keys (AWS Secrets),
  retries with backoff; `error!(alert_id = "tx-failed-vault-messenger")`
  per the tx-alerting convention; produces periodic `StateSync`
  attestations (raw reads only — balances via RPC, `coreState()` via
  eth_call) consumed by the appraisal crank.
- **cctp-relay v2/EVM extension** for auto-completing CCTP legs; Across
  fills are relayer-driven on Across's side (our service only watches for
  arrival to emit/observe `BridgeReceipt`).
- Appraisal/crank flow gains the spoke legs: price attestations (incl.
  USDG/USD) + `record_spoke_state` per spoke + existing legs.
- Oracle service: add a USDG/USD feed; add the HyperCore valuation adapter
  (raw `integration_raw` → value, using the same attested marks discipline
  as existing price sources).

## 9. Config: testnet → mainnet is a flip

All network-dependent values live in per-network config profiles, never in
code — the repo's existing pattern (`config.staging.toml`/`config.prod.toml`
with a `network` profile carrying chain IDs/addresses, as cctp-relay
already does; `rust-backend/deployments.json` for published contract IDs;
frontend per-network maps in `config.ts`). New surfaces follow it:
- Per-chain: RPC URLs, chain IDs, spoke vault + endpoint + adapter
  addresses, asset addresses (USDG, USDC), CCTP domains/state objects,
  Across SpokePool addresses, HyperCore precompile addresses.
- Hub: package/registry object IDs per Sui network.
- One `network_set` selector (testnet-set vs mainnet-set) chooses the whole
  coherent bundle; staging runs the testnet set, prod the mainnet set, and
  promotion is a config/deploy change with zero code edits.

## 10. Build order

Each phase lands green before the next. Testnets: Sui testnet (existing
staging), Robinhood testnet, HyperEVM testnet — all via the §9 testnet set.

1. **Message schema + golden fixtures** (Rust crate + Move + Solidity
   codecs). Verify: cross-codec fixture tests.
2. **Hub Move** — `multichain` module, `EndpointRegistry`,
   `endpoint-attestor`, tranche-aware ledger, appraisal spoke legs,
   payables/in-flight, handlers, ConfigSync events, USDG oracle pin.
   Verify: `sui move test` incl. replay/staleness/refund-deadline/tranche
   gate cases.
3. **`SpokeVault` + `AttestorEndpoint`** in `evm-contracts/`; whitelist,
   deposits, reclaim, per-tranche withdraw queue, gates. Verify: Foundry
   tests. Deploy Robinhood testnet.
4. **`vault-messenger`** + StateSync + crank integration. Verify: e2e on
   Robinhood testnet — USDG deposit → ACK → tradable; withdraw → ACK →
   paid; kill-relayer chaos (reclaim after timeout; freeze on stale
   heartbeat).
5. **HyperEVM spoke + `HyperCoreIntegration`** + hub-side HyperCore
   valuation adapter. Verify: e2e — deposit → ACK → `toCore` → order on
   HyperCore testnet → raw state in `StateSync` → hub NAV moves;
   payout-queue gate blocks `toCore` while a withdrawal is owed.
6. **Bridge adapters** — hub `bridge_out`/receive sweep + `CCTPAdapter`
   (needs Sui CCTP v2 on testnet — re-verify first) + `AcrossAdapter`
   (BLOCKED on §6.3 decision) + cctp-relay v2/EVM. Verify: hub↔HyperEVM
   round trip and Robinhood↔HyperEVM round trip with in-flight accounting
   checked at each step; two-hop Robinhood→HyperEVM→hub rebalance.
7. **`WormholeEndpoint`** for the Sui↔HyperEVM lane.
8. **Frontend + deployment registration + staging rollout**; runbook
   (attestor key rotation, spoke freeze/unfreeze, stuck-message and
   stuck-bridge recovery).

## 11. Open items

- **§6.3 bridge selection for the Robinhood lanes** — Evan validating
  Across (or an alternative); gates phase 6 only.
- **Attestor key ops** — 1 key vs k-of-n at launch; custody/rotation
  procedure. (Why a key exists at all: §2.2.)
- **Spoke share transferability** (ERC-20) — deferred; needs hub
  round-trips or hub-authoritative balance proofs.
- **Cross-chain queue fairness** under stress and a permissionless
  force-repatriation backstop (spoke analog of `begin_force_session`) —
  deferred; MVP relies on the payout-queue trading freeze.
- **Terminal close/settlement**: MVP forbids `initiate_close` while any
  spoke has non-zero assets/payables; drain and unbind first.
- **In-flight bridge loss/timeout handling** — ops runbook item; NAV holds
  the in-flight leg until receipt.
- Accepted MVP risks (signed off 2026-08-29 unless revisited): dark spoke
  blocks vault-wide NAV (safe direction); trusted attestor until Wormhole
  lane ships (permanent for Robinhood); no direct Robinhood↔hub bridge
  lane (two-hop only).
