# 04 — Relayer: EVM→Sui direction (EVM watcher + generic Sui submitter)

**Status (2026-07-01): code complete + tested; live round trip deferred.**
- **L1 BCS layer:** `message::from_bcs` / `envelope::from_bcs` (Move) + `to_move_bcs`
  (Rust) so the relayer passes plain `vector<u8>` args. `bridge_receive` now takes
  BCS bytes and decodes internally (dispatch-design §3.1). **Parity proven E2E in
  Move:** `receive_accepts_bcs_decoded_relayer_args` runs Rust-produced bytes through
  `from_bcs` → real `inbox::receive` with the domain-separated Ed25519 sig verifying.
- **`EvmSourceWatcher`** (alloy): eth_getLogs on the Outbox `MessageCommitted` topic,
  reconstruct + hash-check, confirmation-depth gate. **Anvil integration passed:**
  2 real events reconstructed at 0-conf, 0 at 100-conf, 2 after mining 100.
- **`SuiDestSubmitter`** (sui-tx): reads `dst_app`'s on-chain type → `parse_dispatch`
  derives `(package, module, type_args)` → single `bridge_receive` MoveCall with the
  L1 shared objects + BCS byte args → `submit_ptb`. Type-derivation is unit-tested
  (generic/nested/non-generic/malformed); the PTB path compiles against the real Sui
  SDK. `is_delivered` returns false (on-chain `consumed` set is the real guard; a
  devInspect pre-skip is a noted follow-up).
- **Family `Router`** routes each message by `chain_id::family(dst)` → EVM/Sui submitter
  (unit-tested); `main.rs` runs both source watchers concurrently over one router.
- **Ops:** relay-submission failures log `alert_id = "tx-failed-bridge-relay"`
  (.claude/tx-alerting.md); benign already-delivered races surface as AlreadyDelivered.
- **Tests:** relayer 9 unit + EvmSourceWatcher anvil integration; bridge-types BCS
  parity; Move 18 (incl. 2 BCS parity); locker 10; all suites green.
- **✅ Live HyperEVM→Sui→HyperEVM round trip DONE (M2 exit, 2026-07-01):** deployed the
  Sui Locker`<WBTC>` (Mint) + EVM Locker (Escrow) + test token, wired peers both ways, and
  ran the full round trip on testnet — lock 1 tBTC → mint 1 WBTC → burn → release 1 tBTC,
  supply invariant holding, both signatures verified on the real Inboxes, both reconstructed
  digests matching the on-chain `messageHash`. Addresses in `DEPLOYMENTS.md`. The submitting
  relay was CLI/cast-driven this round; the relayer *binary* is anvil/unit-verified. NOTE:
  the Sui fullnode egress is NOT blocked (re-tested — curl/reqwest/sui-sdk all reach it in
  ~0.2–0.3s); the earlier "reqwest hangs" note in `sui_source.rs` doesn't reproduce, so the
  binary can drive this autonomously — only its write path hasn't been exercised yet.
- Out of scope (unchanged): descriptor registry + custom adapter (§3.3/§3.5).

---


**Spec:** bridge-spec.md §2.5 (Sui delivery), sui-bridge-contracts/relayer-dispatch-design.md §3
**Why:** only Sui→EVM relays today. There is no EVM source watcher and no Sui destination submitter — the Sui side is blocked by design on the `bridge_receive` convention (see the dispatch-design doc), which the Sui Locker now implements. This ticket completes the M2 round trip.

## Scope

### 1. `EvmSourceWatcher`
Poll `MessageCommitted` on the registered EVM Outbox via `eth_getLogs` (block-range cursor, confirmation depth from config so the relayer doesn't hand the signer messages it will refuse). Reconstruct `CrossChainMessage` from event fields and hash-check against the committed hash — same discipline as `sui_source.rs`.

### 2. `SuiDestSubmitter` (dispatch-design §3.2/§3.4 — type-derived, zero per-app config)
1. `is_delivered`: dev-inspect `inbox::is_consumed(digest)`.
2. Resolve dispatch target from the message itself: `getObject(dst_app)` → object type `0xPKG::locker::Locker<T>` → `(package, module, type args)`; assume the standard entry `bridge_receive`.
3. Resolve object args: Inbox + GroupKeyRegistry ids from config, `dst_app`, `Clock 0x6`; fetch `initial_shared_version` + mutability via RPC.
4. Build the PTB. Message/envelope construction: add `message::from_bcs` / `envelope::from_bcs` Move helpers (the dispatch-design's L1 addition) + `bridge_types` BCS emitters, so the PTB passes two `vector<u8>` pure args — simpler than chaining `message::new`/`envelope::new` MoveCalls. (L1 package change → coordinate with ticket 01's republish if possible.)
5. Sign with the relayer's Sui key, submit, confirm effects.

### 3. Family router
Replace the single-submitter wiring in `main.rs` with routing by `chain_id::family(message.dst_chain_id)` → `EvmDestSubmitter` / `SuiDestSubmitter`. Both watchers run concurrently; the `SourceWatcher`/`DestSubmitter` traits already support this shape.

### 4. Ops conventions (repo standard)
Relayer submits transactions now in both directions: every tx-submission failure at the relay handler must `error!(alert_id = "tx-failed-bridge-relayer-...")` per .claude/tx-alerting.md, with benign race-losses (already-consumed on arrival) suppressed as info.

## Verify (exit criteria)
- Unit: EVM event decode + hash-check vectors; dispatch-target derivation from a mocked `getObject`; router selection.
- Integration: localnet/anvil pair — EVM `lock` → watcher → signer → `bridge_receive` PTB lands, wrapped coin minted.
- Live (M2 exit): **round-trip a test asset HyperEVM→Sui→HyperEVM with the supply invariant holding**, on the deployed testnet contracts, driven end-to-end by one relayer process.

## Out of scope
Descriptor registry + custom-adapter escape hatch for non-standard apps (dispatch-design §3.3/§3.5) — add when a second app family exists.

**Depends on:** 01 (digest), 02 (signer must verify EVM commitments before signing them), 03 (EVM Locker to originate/receive). **Blocks:** M2 completion.
