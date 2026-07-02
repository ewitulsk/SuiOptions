# 01 — DOMAIN_SEP in the message digest (+ redeploy)

**Spec:** bridge-spec.md §2.2 (mandatory domain separation)
**Why now:** the deployed digest is a bare `keccak256(encode(message))` on all three implementations. Every testnet redeploy (fresh `consumed` set, same registry IDs) lets previously signed messages replay. Changing the digest later invalidates all accumulated signatures and touches every layer — do it before more traffic and before the other tickets land on the wrong format.

## Design decisions (locked)

- **Derivation:** `DOMAIN_SEP = keccak256("XCHAIN_MSG_V1" || deployment_salt)`, `message_hash = keccak256(DOMAIN_SEP || encode(message))`. Contracts take the 32-byte `deployment_salt` at construction and derive `DOMAIN_SEP` once on-chain (auditable, no mis-derived constants). Services take the salt in config and derive identically.
- **Storage:** one salt per logical deployment, shared by both chains and both services.
  - Solidity: `bytes32 immutable domainSep` on `Outbox` and `Inbox` (constructor arg).
  - Move: `domain_sep: vector<u8>` field on `Outbox` and `Inbox`, set in the governance-gated `create(...)`.
  - Rust: `deployment_salt` in signer-service and relayer configs.
  - A cross-chain mismatch self-surfaces (signatures don't verify) — no extra consistency machinery.
- **Salt for this testnet deployment:** `keccak256("sui-options-bridge:testnet:2026-07")`, recorded in DEPLOYMENTS.md. Test vectors use a fixed dummy salt (`[0x01; 32]`).
- **Rust API:** change `CrossChainMessage::digest()` to `digest(domain_sep)` — deliberately breaking so no saltless call sites survive.

## Steps

1. **Rust `bridge-types` → generate the new parity vector.** Add `derive_domain_sep(salt)`; thread `domain_sep` through `digest()`. Update `known_digest_vector` with the test salt; the captured digest becomes the vector Move + Solidity must reproduce.
   *Verify:* `cargo test -p bridge-types`.
2. **Move package.** `message::hash(m, domain_sep)`; `outbox::create` / `inbox::create` take `deployment_salt`, store the derived sep; `send`/`receive` use it. Fix the stale "windowed ordering" doc comment in `inbox.move` (behavior is dedup-only). Update the Move parity test.
   *Verify:* `sui move test`.
3. **Solidity package.** `Message.hash(m, domainSep)`; immutables + constructor args on `Outbox`/`Inbox`; `Deploy.s.sol` reads the salt from env. Fix the same stale comment in `Inbox.sol`. Update the parity test.
   *Verify:* `forge test`.
4. **Services.** `bridge-signer::sign` holds the sep; signer-service + relayer configs gain `deployment_salt`; the relayer's event decoder hash-check uses it. Update both `config.example.toml`.
   *Verify:* `cargo test -p bridge-signer -p bridge-signer-service -p bridge-relayer`.
5. **Republish + rewire.** Changing `public fun hash`'s signature violates Sui upgrade compatibility → **fresh publish** of `sui_bridge`; rebuild `sui-locker` against the new dep (no code change — it never hashes). Fresh EVM deploy with the salt. Re-register both chains + the **existing** group keys (no rotation needed — the new domain already invalidates old signatures). Update DEPLOYMENTS.md with new IDs + salt.
   *Verify:* deploy output matches DEPLOYMENTS.md; live signer `/group_keys` matches what's registered.
6. **End-to-end smoke** (mirror the 2026-06-29 procedure). `Outbox.send` on new Sui Outbox → relayer reconstructs + hash-checks → signer envelope → submit to HyperEVM Inbox (anvil fork first, then live). Negative test: a signature over the old unsalted digest must be rejected.
   *Verify:* delivered + consumed on HyperEVM testnet; old-digest replay reverts.
7. **Spec bookkeeping.** Mark §2.2 implemented; adopt the packed-layout wording; note in §9-resolved.

## Out of scope
RPC verifier (02), EVM Locker (03), queue (05), async API (06).

**Depends on:** nothing. **Blocks:** all other contract-touching tickets (build on the final digest).
