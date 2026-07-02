# 10 — Lifecycle, governance & hardening (M4 / audit-ready)

**Spec:** bridge-spec.md §8 M4, §9 open items
**Why:** everything before this ticket makes the bridge work; this ticket makes it operable and auditable. Collects the deliberately-deferred decisions.

## Scope

### 1. Governance & guardian wiring (§9.8)
- Decide + implement key topology: who holds `GovernanceCap`/`GuardianCap` (Sui) and governance/guardian roles (EVM) — multisigs, not deployer EOAs/addresses (today both are the deployer, see DEPLOYMENTS.md).
- Operator-cap custody for `Enclave` registration caps (§9.4): hardware wallet vs per-operator multisig. Document per operator.
- Pause drill: guardian pauses/unpauses each of the 4 boxes + a Locker from the real multisig, runbooked.

### 2. HyperEVM finality confirmation (§9.1)
Confirm HyperEVM finality semantics + the dual-block architecture (small vs big blocks, separate gas limits) against current Hyperliquid docs. Set the confirmation depth in the registries + verifier config from evidence, not the placeholder 12. Verify which block type our Inbox/Locker txs land in and the gas implications.

### 3. Mainnet Seal key-server set (§9.5, §6.6)
Choose vetted independent operators at t ≥ 2 (or committee mode); establish availability agreements; re-encrypt share ciphertexts to the chosen set.

### 4. Rotation & recovery runbooks (§6.4)
- Membership rotation / proactive re-share drill on testnet: re-share (fixed group key) AND full rotation (fresh DKG + `registerGroupKey` + retire old id). 
- Node-replacement drill end-to-end (ticket 08 flow) with timing targets.
- Relayer economics decision (§9.6): v1 = self-relay (we run the relayer, eat destination gas) — confirm and document, or design fees.

### 5. Observability & alerting
- Per repo convention (.claude/tx-alerting.md): every service tx-submission failure carries an `alert_id`; benign races suppressed. Cover: relayer submits (both chains), signer availability, verifier provider disagreement, `Enclave` re-registration events, rate-limit-queue growth, supply-invariant monitor (wrapped supply vs escrow, cross-chain — the one alert that catches everything else failing).
- Dashboards + alert rules through the existing Prometheus/Grafana stack.

### 6. Security review (§8 M4)
External audit scope: Move packages (messaging, locker, seal-policy, enclave registration), Solidity (messaging + locker), the Nautilus fork delta (upstream is explicitly unaudited), the chosen MPC libraries + our integration (session handling, abort behavior), and the DKG ceremony procedure. Internal pre-audit pass: adversarial test suite covering every trust-table row in bridge-spec §1.

## Verify (exit criteria — spec M4 exit)
- Runbook-complete: every operational procedure (pause, rotate, re-share, replace node, rewind relayer cursor) executed at least once on testnet by the person who'd do it in production.
- All governance actions require the real multisigs.
- Alert coverage demonstrated by fault injection (kill signer, wedge relayer, force provider disagreement, over-limit transfer).
- Audit engagement scoped and scheduled; findings triaged to closure before any mainnet value.

**Depends on:** effectively everything (01–09). **Blocks:** mainnet.
