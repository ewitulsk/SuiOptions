# 10 — Lifecycle, governance & hardening (M4 / audit-ready)

**Spec:** bridge-spec.md §8 M4, §9 open items · **Status:** not started · **Depends on:** effectively 01–09 · **Blocks:** mainnet
**Why:** everything before this makes the bridge *work*; this makes it *operable, governable, and auditable*. It collects the decisions deliberately deferred through 01–09 and turns the demo posture (deployer EOAs, demo keys, placeholder finality) into production posture.

---

## Detailed implementation plan

### 1. Governance & guardian topology
Today both chains sit on deployer EOAs (`0xab8d…` on Sui, `0x303c…` on EVM — see DEPLOYMENTS.md) and the round trip used **demo group keys whose seeds are in the repo history** (`[0x42]`/`[0x11]`). Production:
1. **Sui:** transfer `GovernanceCap` + `GuardianCap` to multisig-controlled addresses (Sui multisig or a governance object). **EVM:** point Registry `governance`/`guardian` at a Safe (or equivalent). Decide the topology (m-of-n, who) per role — guardian (pause, fast) is usually a smaller/faster set than governance (registry/keys/threshold).
2. **Operator-cap custody (§9.4):** each node's `Enclave`-registration cap (ticket 07) is the per-node credential. Decide hardware-wallet vs per-operator multisig, document per operator. Stealing this cap = pointing that node's identity at an attacker enclave, so it's as sensitive as a signer key.
3. **Retire the demo keys:** the live testnet group keys must be replaced by the ticket-09 DKG keys (or, if staying 1-of-1 longer, keys whose seed is in the enclave/Seal, never the repo). `registerGroupKey` a real id, retire id 1/2.
4. **Pause drill:** from the *real* multisig, pause + unpause each of the 4 boxes (Sui/EVM Inbox+Outbox) and a Locker; runbook it.

### 2. HyperEVM finality confirmation (§9.1) — the placeholder we've been carrying
Confirm HyperEVM finality semantics + the **dual-block architecture** (frequent small blocks vs ~1/min big blocks, separate gas limits) against current Hyperliquid docs. Then:
- Set the confirmation depth in the chain registries + the signer's `confirmations` config from **evidence, not the placeholder `12`**.
- Determine which block type our Inbox/Locker txs land in and the gas implications.
- Revisit the ticket-04 finding: the signer's `EvmProbe` uses a bounded `lookback_blocks` window; make sure that window comfortably exceeds `confirmations` + realistic relay delay, and note public-RPC `eth_getLogs` range caps (we hit `max block range 1000`).

### 3. Production RPC & relayer economics
- **Dedicated RPCs:** the live run showed the shared `rpcs.chain.link` endpoint rate-limits (`ErrUpstreamsExhausted` / HTTP 500) under sustained relayer polling. Production needs dedicated/authenticated RPC endpoints for both the relayer watchers and the in-enclave verifier quorum — and the verifier needs **≥2 independent** ones (§5.4), so budget for that.
- **Relayer economics (§9.6):** v1 = self-relay (we run the relayer, eat destination gas). Confirm + document, or design a fee mechanism. Also implement the deferred `is_delivered` for the Sui submitter (a `devInspect` `inbox::is_consumed` check) so re-deliveries pre-skip instead of failing a tx.

### 4. Mainnet Seal key-server set (§9.5, §6.6)
Testnet uses Mysten's open servers at t=1. For mainnet: choose vetted independent operators at **t ≥ 2** (or a committee-mode MPC key server), establish availability/SLA agreements (Seal's own "treat key-server selection as a trust decision"), and re-encrypt every node's share ciphertext to the chosen set. Remember: a colluding quorum of `t` key servers can derive any share ciphertext — this layer sits *above* the bridge's k-of-n.

### 5. Rotation & recovery runbooks (§6.4) — drilled, not just written
Each executed once on testnet by the person who'd do it in prod:
- **Re-share** (rotate shares, group key fixed) AND **full rotation** (fresh DKG → `registerGroupKey` new id → retire old).
- **Node replacement** end-to-end (ticket-08 flow: fresh enclave → operator re-register → Seal reload → resume), with a timing target.
- **Relayer cursor / stuck-message** recovery (the EVM watcher `from_block` cursor, the dedup-only redelivery behavior).
- **Emergency:** guardian pause → investigate → unpause.

### 6. Observability & alerting (repo `.claude/tx-alerting.md` convention)
Every service tx-submission failure carries an `alert_id` (already done in the relayer: `tx-failed-bridge-relay`). Extend coverage:
- Signer availability / verifier **provider disagreement** (a split quorum vote is a strong tamper signal).
- **`Enclave` re-registration** events (anomalous ones = attack signal).
- **Rate-limit-queue growth** on both Lockers (queued transfers piling up).
- **The one alert that catches everything:** a cross-chain **supply-invariant monitor** — `wrapped_supply_on_foreign ≤ locked_collateral_on_home` per asset per route. If this ever breaks, something upstream (signer, verifier, a contract bug) already failed. Wire it into the existing Prometheus/Grafana + balance-monitor stack.

### 7. Security review (§8 M4)
- **External audit scope:** Move packages (messaging, locker, seal-policy, enclave registration), Solidity (messaging + locker), the **Nautilus fork delta** (upstream is explicitly unaudited — hence the `FORK_DELTA.md` from ticket 07), the chosen MPC libraries + our integration (session handling, abort behavior, the no-reconstruction invariant), and the DKG ceremony transcript/procedure.
- **Internal pre-audit pass:** an adversarial test suite that has a red-team case for **every row of the bridge-spec §1 trust table** (message authenticity, share confidentiality, honest-code attestation, no reconstruction, liveness, replay, source truth, chain-view integrity, per-node isolation, key-server honesty).

## Exit criteria (spec M4 exit)
- Runbook-complete: every operational procedure above executed at least once on testnet by its real operator.
- All governance/guardian actions require the real multisigs; demo keys retired.
- Alert coverage demonstrated by **fault injection** (kill a signer, wedge the relayer, force a provider disagreement, push an over-limit transfer, attempt an `Enclave` re-register).
- Audit engagement scoped + scheduled; findings triaged to closure **before any mainnet value flows**.

## Effort & sequencing
Spread across the M3 work rather than a single block — items 1/2/6 can start as soon as the relevant pieces exist; 7 (audit) is the long-pole calendar item, so scope + book it early. This ticket is "done" when the bridge could carry real value with a straight face.
