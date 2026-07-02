# 04 — Relayer: EVM→Sui direction (EVM watcher + generic Sui submitter)

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
