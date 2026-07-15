# siws_session — Move package

On-chain half of the SIWS session-key system (see `../sui-siws-session-key-spec.md`).

## Modules

| Module | Role |
|--------|------|
| `registry` | Shared singleton (created at publish). `solana_pk → Account` map + consumed-nonce set. Holds the `domain` (its own address) and `network` tag baked into every signed message. |
| `account` | Per-user shared treasury: `Balance<T>`, `generation` (bump to revoke all caps), and a per-cap cumulative spend ledger. |
| `session` | `SessionCap` type, `verify_and_open_session` (ed25519 / Solana) and `verify_and_open_session_eth` (secp256k1 / EIP-4361), `revoke_all` + `revoke_all_eth`, and the shared `authorize` helper that enforces holder/account/generation/expiry/per-tx/total/allowlist on every app call. Account `owner_pk` holds a 32-byte ed25519 pubkey or a 20-byte eth address — scheme inferred by length. |
| `message` | Canonical Solana (SIWS) message serializer. Rebuilds the signed bytes from checked args — never trusts a caller blob. Byte-exact with `sdk/src/message.ts`, pinned by tests on both sides. |
| `siwe` | EIP-4361 (Sign-In With Ethereum) message builder + EIP-191 prefix + secp256k1 `ecrecover` → keccak address derivation (incl. EIP-55 checksum). Byte-exact with `sdk/src/siwe.ts`; pinned against a real-signature reference vector. |
| `app_example` | Example scoped entrypoint (`withdraw`) showing how a dApp delegates cap checks to `session::authorize`. |
| `errors` | Stable error codes (spec §1.8), referenced by the SDK. |

## Message format

ASCII, single `\n` separators, no trailing newline (spec §1.4). Lowercase hex
(not base58) so both sides stay trivially byte-exact:

```
siws-session-v1
domain: 0x<registry addr>
chain: sui:<network>
account: 0x<solana ed25519 pubkey>
session_key: 0x<temp sui addr>
generation: <u64>
nonce: 0x<32-byte nonce>
expires_at_ms: <u64>
```

## Build / test / publish

```bash
sui move build
sui move test
sui client publish --gas-budget 200000000
```

After publishing, note the **package id** and the **Registry** object id (a
shared object created by the module initializer). Feed both to the SDK config.
