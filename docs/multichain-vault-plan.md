# Multichain Trading Vault: Sui Hub / EVM Spokes

Status: DESIGN — reviewed with Evan 2026-08-28/29; descoped 2026-08-29 (cut
Hyperliquid entirely; cut all vault-level bridging). Not yet implemented.

## Summary

Extend trading-vault-v2 to a hub-and-spoke multichain vault. MVP scope after
descope:

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
- **No funds move between hub and spoke.** All vault-level bridging (CCTP,
  Across) is cut from MVP. Spoke deposits stay on the spoke; hub assets
  stay on the hub. Both sides count in one global NAV.
- **Curator controls funds on both sides.** On the hub, the existing
  session/adapter machinery is unchanged. On the spoke, the vault ships a
  **curator-controlled integration interface** (§6) — the only way funds
  can ever leave the spoke vault besides user withdrawals. **MVP registers
  zero integrations**, so spoke funds are inert until one is added; a
  future bridge is just another integration behind this interface.
- **Messaging is transport-agnostic**; the MVP transport is our attestor
  (required for Robinhood regardless — no third-party messaging serves it).

### Locked decisions (2026-08-28/29 reviews)

1. Messaging: agnostic endpoint interface; attestor transport for MVP.
2. Accounting: hub is master; spokes report raw quantities only — no
   valuation, NAV, or share math on any spoke, ever.
3. Withdrawals in scope, hub-directed and share-denominated (spoke asks,
   waits, hub answers "pay user X amount A").
4. Spoke depositor gating: a separate, per-chain whitelist on each
   `SpokeVault` (the hub's Sui `Whitelist` is not consulted).
5. Robinhood spoke deposit asset is **USDG** (no liquid USDC there).
6. **Tranching is supported on spoke vaults**; all tranche accounting stays
   on the hub — the spoke relays the user's tranche choice as an opaque
   code.
7. Testnet-first; every network-dependent value in config profiles so
   mainnet is a config flip (§9).
8. **2026-08-29 descope:** Hyperliquid (spoke, HyperCore/CoreWriter,
   Wormhole lane) cut entirely — nothing ships on Hyperliquid yet.
   Vault-level bridge integrations (CCTP, Across) cut — no hub↔spoke fund
   transfer path in MVP. The curator integration interface (§6) stays, with
   zero registered integrations.

### External facts this design depends on (verified 2026-08-29)

- Robinhood Chain: EVM (Arbitrum Orbit), mainnet 2026-07-01, testnet since
  2026-02-10. No Wormhole/LayerZero/CCTP → our attestor transport is
  required. Chainlink is live there (unused by us — valuation is hub-side).
- **USDG** (Paxos Global Dollar) is live on Robinhood Chain. USDG does
  **not** exist on Sui — one reason no hub↔spoke transfer lane exists even
  if we wanted one; a future bridge integration must handle asset
  transformation.
- Testnet USDG availability on Robinhood testnet is unconfirmed (open item
  — may need a mock ERC-20 for staging).

## 1. Roles and fund states

Hub: existing `vault_v2::TradingVault`, extended with a spoke ledger (§3).
Untranched and Senior/Junior vaults both supported.

Spoke vault fund states (tracked per asset; MVP: USDG only):

| State      | Meaning                                            | Curator can touch? |
|------------|----------------------------------------------------|--------------------|
| `pending`  | deposit escrowed, no hub ACK yet                   | NO                 |
| `active`   | hub-ACK'd; part of vault NAV                       | via integrations¹  |
| `reserved` | owed to a hub-ACK'd withdrawal (payout, §5)        | NO                 |

¹ MVP registers no integrations, so `active` funds sit in the vault and
back withdrawals; the interface (§6) is how curator control materializes.

The pending→active transition happens ONLY on a hub `DepositAck` — the
"funds unusable until ACK" invariant, enforced on-chain on the spoke.

**Spoke depositor whitelist**: each `SpokeVault` carries its own allowlist
(admin-managed); `deposit` and `requestWithdraw` check it. Empty = open.
The hub does not re-check identity for spoke ledger entries.

**Hub valuation of spoke assets**: the hub's accounting asset stays USDC.
USDG is valued through the existing `PriceAttestation` machinery — a
USDG/USD feed added to the oracle service and pinned in `OracleRegistry`,
like any other non-accounting deposit asset. No 1:1 assumption.

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
  shares, all: bool }`
- `PayoutReceipt { spoke_id, request_seq, amount }`
- `StateSync { spoke_id, per-asset {free, reserved}, integration_raw, ts }`
  — `integration_raw` is raw venue data from any future integration (empty
  in MVP), never a computed value; the hub turns raw data into value.

Hub → Spoke (instructions):
- `DepositAck { deposit_seq, accepted: bool, shares }` — `shares` is
  hub-computed; the spoke records it verbatim for its UX mirror.
- `WithdrawAck { request_seq, user: bytes32, pay_amount }`
- `ConfigSync { paused, risk_off, curator: address, integrations_root }`

### 2.2 Why an attestor key (and hub-side transports)

The hub cannot observe Robinhood Chain. When a message claims "user X
deposited N", the hub needs on-chain-verifiable proof or anyone could mint
shares by calling the handler. On lanes served by Wormhole/LayerZero that
proof is the transport's validator signatures; no transport serves
Robinhood, so the proof must be OUR signature: the attestor key. (Gating on
the relayer's tx-sender address is the same trust with less flexibility — a
signed-message design verifies identically on Sui and EVM, supports k-of-n,
and rotates without changing gas payers. It matches the existing
`registrar_pubkey` / hedge-signer patterns.)

Each transport is a witness-typed Move module allow-listed in a new
`EndpointRegistry` (mirroring the oracle/integration adapter pattern). A
transport verifies its own proof and constructs a `VerifiedMessage` hot
potato that only `vault_v2::multichain` can consume; `multichain` checks
the spoke binding (`spoke_id → (chain_id, spoke_vault_address,
endpoint_type)`) and `seq`, then applies the payload. Outbound:
`multichain` emits a canonical `OutboundMessage` event; the attestor
transport relays it. Third-party transports (Wormhole etc.) remain future
endpoint modules for chains that have them — post-MVP.

- `endpoint-attestor` (MVP): ed25519 signature(s) over
  `domain_tag ‖ envelope ‖ payload`, pubkeys in `EndpointRegistry`. Start
  with 1 key, structured for k-of-n. Key custody/rotation: ops item.

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
    via its feed) plus `integration_value`, minus `payables`. NAV cannot
    complete with a stale spoke → the safe failure mode.
- **Message handlers** (consume `VerifiedMessage`; valuation happens here,
  against a complete `Appraisal`):
  - `handle_deposit_notice`: vault open + not paused + tranche valid →
    value `amount` at the attested price, apply entry haircut, mint shares
    into the requested tranche at current NAV, record Holding, bump spoke
    `free` → `DepositAck{accepted: true, shares}`. Rejections →
    `accepted: false` (spoke refunds).
  - `handle_withdraw_request`: validate holding + lockup + the same
    per-tranche gates hub withdrawals face (junior blocked in risk-off
    states, generation wipe checks), value shares at current NAV with exit
    haircut, **cap by spoke solvency (§5.1)**, crystallize perf fee vs
    basis, burn the shares actually honored, `payables += pay_amount` →
    `WithdrawAck`. Rejected → `pay_amount: 0`.
  - `handle_payout_receipt`: clear payable, deduct spoke balance.
- **Gate propagation**: `ConfigSync` pushed on pause, risk-state flips from
  `sync_capital`, curator rotation, integration-set changes. Spokes treat a
  stale heartbeat (> N min) as `risk_off = true` locally — integrations
  freeze; deposits and payouts continue.

## 4. Deposit flow (spoke)

1. User (spoke-whitelisted): `SpokeVault.deposit(asset, amount, tranche)` →
   escrowed `pending`, local `deposit_seq`, `DepositNotice` sent. The
   spoke validates nothing about the tranche beyond range — the hub owns
   policy.
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

## 5. Withdrawal flow (spoke) — hub-directed

The hub is master; the spoke asks and waits. Requests are
**share-denominated** (`shares` or `all`, per tranche); dollar estimates
are a UI concern served from hub NAV via the API — the spoke computes
nothing.

1. User: `requestWithdraw(tranche, shares | all)` → spoke records the
   request (one in-flight per (user, tranche)), sends `WithdrawRequest`.
2. Hub: validates against the ledger, enforces lockup + tranche gates,
   prices at current NAV with exit haircut, applies the solvency cap
   (§5.1), crystallizes fees, burns the honored shares, books `payables` →
   `WithdrawAck{user, pay_amount}` denominated in the spoke's deposit
   asset at attested prices.
3. Spoke on ACK: move `pay_amount` to `reserved` and pay the user. Under
   §5.1 the ACK never exceeds what the spoke holds, so payment is
   immediate; `reserved` exists for the gap between ACK arrival and the
   payout tx, and integration calls (once any exist) revert while any ACK
   is unpaid.
4. On each payment: `PayoutReceipt` → hub clears the payable.

### 5.1 Spoke solvency cap (consequence of "no bridging")

With no hub↔spoke transfer path, a spoke can only ever pay out what it
locally holds — but spoke shares are claims on GLOBAL NAV. If the vault
appreciates from hub-side trading, spoke claims can exceed the spoke's
local USDG. Burning shares the spoke cannot pay would strand users behind
an unfundable queue.

Rule: when handling a `WithdrawRequest`, the hub honors
`min(requested value, spoke free − outstanding payables)` and burns only
the shares corresponding to the honored amount; **unhonored shares stay
outstanding** (still earning/losing with the vault, hurdle still accruing
for senior) and the remainder of the request is queued hub-side, retried at
each subsequent crank as spoke liquidity appears (new deposits, or a future
bridge/integration returning funds). The spoke never receives an ACK it
cannot pay. Partial honoring is FIFO per spoke, and the same tranche gates
apply on every retry.

Consequences to be aware of (accepted 2026-08-29 unless revisited):
- If withdrawal demand exceeds cumulative spoke inflows, the tail waits
  indefinitely until a bridge integration exists. Deposits keep working.
- Terminal close (`initiate_close`) with a spoke whose claims exceed local
  funds cannot fully drain that spoke without a bridge; MVP forbids close
  while a spoke has non-zero assets or payables.

## 6. Curator integration interface (spoke)

The only path for funds to leave a `SpokeVault` other than user payouts.
Ships in MVP as an interface with **zero registered integrations**.

```solidity
interface ISpokeIntegration {
    // funds flow vault -> integration only via SpokeVault.extendTo:
    // curator-only, active-funds-only, blocked while any ACK is unpaid
    // and while risk_off/stale-heartbeat; integration must be registered.
    function onFundsReceived(address asset, uint256 amount) external;
    // raw state for StateSync.integration_raw (never a valuation)
    function rawState() external view returns (bytes memory);
    // permissionless return path: anyone can push funds back to the vault
}
```

- **Registration is hub-governed**: the integration set for a spoke is
  committed on the hub (admin + curator co-signed, like adapter
  allow-listing) and propagated via `ConfigSync.integrations_root`; the
  spoke accepts only registered integrations. Removal is an instant kill
  switch for new `extendTo` calls, mirroring `IntegrationRegistry`
  semantics on the hub.
- Every integration must expose `rawState()` so the hub can value deployed
  funds; a hub-side valuation adapter (oracle-registry slot) is part of
  registering any integration.
- A future bridge (CCTP, Across, …) is just an integration under this
  interface with destination restricted to registered vault addresses,
  plus in-flight accounting on the hub — designed then, not now.
- Curator identity on the spoke comes from `ConfigSync` (rotation
  propagates from the hub).

## 7. Spoke contracts (new `evm-contracts/` Foundry workspace)

- `SpokeVault.sol` — per-asset fund states + depositor whitelist +
  deposit/reclaim + per-tranche withdraw + `handleMessage` dispatch +
  endpoint binding + curator address + integration registry (§6) +
  pause/stale-heartbeat freeze.
- `AttestorEndpoint.sol`.
- Shared message codec library mirroring the Move schema; golden byte
  fixtures keep Move/Solidity/Rust codecs in lockstep.

## 8. Off-chain

- **New `rust-backend/services/vault-messenger`** (patterns: `cctp-relay`
  watcher + `hedge-signer` key handling): watches hub events and spoke
  logs, persists ordered per-lane message queues in Postgres,
  attestor-signs, submits with per-chain relayer keys (AWS Secrets),
  retries with backoff; `error!(alert_id = "tx-failed-vault-messenger")`
  per the tx-alerting convention; produces periodic `StateSync`
  attestations (raw RPC reads only) consumed by the appraisal crank.
  Full 9-spot deployment registration per repo convention.
- Appraisal/crank flow gains the spoke legs: price attestations (incl.
  USDG/USD) + `record_spoke_state` per spoke + existing legs.
- Oracle service: add a USDG/USD feed (source TBD — open item).

## 9. Config: testnet → mainnet is a flip

All network-dependent values live in per-network config profiles, never in
code — the repo's existing pattern (`config.staging.toml`/`config.prod.toml`
with a `network` profile, as cctp-relay does; `rust-backend/deployments.json`
for published IDs; frontend per-network maps in `config.ts`):
- Per-chain: RPC URLs, chain IDs, spoke vault + endpoint addresses, asset
  addresses (USDG or its testnet mock).
- Hub: package/registry object IDs per Sui network.
- One `network_set` selector (testnet-set vs mainnet-set) chooses the whole
  coherent bundle; promotion is a config/deploy change with zero code
  edits.

## 10. Build order

1. **Message schema + golden fixtures** (Rust crate + Move + Solidity
   codecs). Verify: cross-codec fixture tests.
2. **Hub Move** — `multichain` module, `EndpointRegistry`,
   `endpoint-attestor`, tranche-aware ledger, appraisal spoke legs,
   payables + solvency cap, handlers, ConfigSync events, USDG oracle pin.
   Verify: `sui move test` incl. replay/staleness/refund-deadline/tranche
   gate/solvency-cap cases.
3. **`SpokeVault` + `AttestorEndpoint`** in `evm-contracts/`; whitelist,
   deposits, reclaim, per-tranche withdraw, integration registry (empty),
   gates. Verify: Foundry tests. Deploy Robinhood testnet.
4. **`vault-messenger`** + StateSync + crank integration. Verify: e2e on
   Robinhood testnet — USDG deposit → ACK → active; withdraw → ACK → paid →
   receipt clears payable; partial honoring when spoke free < claim value;
   kill-relayer chaos (reclaim after timeout; freeze on stale heartbeat).
5. **Frontend + deployment registration + staging rollout**; runbook
   (attestor key rotation, spoke freeze/unfreeze, stuck-message recovery).

Cut from MVP (return as future phases when wanted): Hyperliquid spoke +
HyperCore/CoreWriter integration; bridge integrations (CCTP/Across) with
in-flight accounting; Wormhole/LayerZero endpoint modules; ERC-20 spoke
shares.

## 11. Open items

- **USDG/USD feed source** for hub valuation (Pyth? our oracle-service
  attesting from market data?) — needed by phase 2.
- **Robinhood testnet USDG** — exists, or deploy a mock ERC-20 for staging?
  Needed by phase 3.
- **Attestor key ops** — 1 key vs k-of-n at launch; custody/rotation.
- **Spoke whitelist + integration-set governance ownership** — which
  key(s) administer the spoke whitelist, and confirm the hub-governed
  integration registration mechanism (§6) before any integration is added.
- **Solvency-cap semantics (§5.1)** — confirm partial honoring + shares
  staying outstanding is the desired behavior (vs. burning in full and
  letting an unfundable queue accrue).
- Accepted MVP limitations (signed off 2026-08-29 unless revisited):
  spoke funds are inert (no integrations registered) and cannot reach the
  hub; withdrawal tails can wait indefinitely if demand exceeds spoke
  inflows; terminal close blocked while a spoke holds assets/payables;
  dark spoke blocks vault-wide NAV (safe direction); trusted attestor is
  the messaging trust root.
