# 03 — P1: `key-service`

Custody of curator keys, server-side signing, password-gated export. New crate `rust-backend/services/key-service`. **Two-port split, exactly like token-info (9005/9006) and auth-service (9007/9008):**

- **:9022 public** — nothing in v1 except `/health` (export goes through the dashboard → provisioner → internal call; if a direct public export endpoint is added later it sits here behind auth). Not nginx-routed until needed.
- **:9023 internal** — `/internal/*` signing + key-management API. **Never proxied, never in nginx.** Only bot-gateway and provisioner call it over the compose network.

```
services/key-service/
  Cargo.toml                      axum, diesel, aws-sdk-kms, ed25519-dalek (or fastcrypto via sui-tx)
  config/config.{toml,staging.toml,prod.toml}
  src/{main,lib,config,router,state}.rs
  src/handlers/{mod,sign,keys,export}.rs
  src/{kms.rs,audit.rs}
  src/db/…  (migrations)
```

## 1. Key model — AWS KMS envelope encryption

One KMS CMK per env (terraform: `aws_kms_key.curator_keys` in `rust-backend/infra`, alias `alias/options-<env>-curator-keys`; grant the EC2 instance role `kms:GenerateDataKey` + `kms:Decrypt` on it — follow the existing IAM wiring for Secrets Manager in `infra/`).

Per curator key:

```
generate ed25519 keypair (in-process)
→ kms:GenerateDataKey (AES-256)              → { plaintext_dk, encrypted_dk }
→ AES-GCM encrypt(bech32 suiprivkey bytes)   → ciphertext + nonce
→ zeroize plaintext_dk + plaintext key
→ store row: (vault_id, address, pubkey, encrypted_dk, nonce, ciphertext)
```

Decryption happens per signing request (`kms:Decrypt` on `encrypted_dk` → AES-GCM open → sign → zeroize). If signing latency matters later, add a bounded in-memory LRU of decrypted keys with TTL — start without it.

The curator key is a standard Sui ed25519 key: it signs **both** order digests (exchange, `exchange_signing` semantics — the exact byte recipe is in `staging-mm-bot/src/signing.rs:31-60`, which extracts the 32-byte seed from `suiprivkey1…` bech32) **and** transactions (`Intent::sui_transaction()` sign-bytes). Ed25519 only — fail closed on any other scheme, same as staging-mm-bot.

## 2. Internal API

```
POST /internal/keys                     { vaultHint } → { keyId, address, pubkeyB64 }
                                        (provisioner, BEFORE vault creation — address is needed
                                         to fund gas and to be tx sender of the wrap ceremony)
POST /internal/keys/:id/bind            { vaultId }   (provisioner, after vault creation)
POST /internal/sign/order               { vaultId, digestB64 } → { signatureB64, pubkeyB64 }
POST /internal/sign/tx                  { vaultId, txBytesB64 } → { signatureB64 }
POST /internal/keys/:id/revoke          ends signing for this key (export flow, D19)
POST /internal/export                   { vaultId, password } → { encryptedKeyB64, kdf: "argon2id", … }
                                        password-wraps the bech32 key; then auto-revokes (D19)
GET  /internal/keys/:id/audit           paged audit rows
```

Authorization between services: a shared bearer token per caller rendered into both services' secrets TOML (`[key_service] gateway_token`, `provisioner_token`) — same trust posture as auth-service's internal `/verify` port. Per-vault authorization: the gateway's calls are checked against the key row's `vault_id`.

## 3. Signing guardrails

- `/internal/sign/tx` **dry-runs and inspects** the tx before signing: every Move call target must belong to the allowlisted set (`bounded_curator::*`, `guarded_exchange_adapter::*`, `sui_tx` cancel/watermark targets, oracle attest calls). Reuse the `PtbTemplate` matcher from `crates/sui-tx/src/tx/template.rs` — build a `key_service_templates()` list with the same `TargetMatcher::Exact` machinery instead of writing a second matcher. A PTB that doesn't match any template is refused and logged with `describe_ptb`.
- `/internal/sign/order`: assert the digest was requested by the gateway for a known open intent (gateway passes the intent id; belt-and-suspenders against a compromised gateway signing arbitrary payloads).
- Post-revoke, all sign endpoints return 410 for that key. Re-hosting = fresh key + owner `rotate_curator` (spec D19).

## 4. Audit log

`signing_audit` append-only table: `(id, key_id, vault_id, kind order|tx|export|revoke, digest, template_name, caller, created_at)`. No UPDATE/DELETE grants for the service role beyond INSERT/SELECT (enforce in the migration). Surfaced read-only in the dashboard records view.

## 5. Export flow (P4, but the API ships in P1 so the schema is stable)

`argon2id(password)` → AES-GCM wrap the bech32 key → return blob; mark key revoked; emit audit `export`. The dashboard walks the user through decryption client-side (the SDK ships a matching `curator_sdk.keys.unwrap_exported_key`). We never see the password (it arrives over TLS for the KDF — if that is deemed unacceptable, move the KDF client-side and have the service return the raw key over an authenticated channel once; decide at P4, the storage schema doesn't change).

## 6. DB schema (`key_service_<env>`)

```
curator_keys   (id, vault_id NULL until bind, address, pubkey, encrypted_dk BYTEA,
                nonce BYTEA, ciphertext BYTEA, created_at, revoked_at)
signing_audit  (see §4)
```

## 7. Config + secrets

```toml
bind_public = "0.0.0.0:9022"
bind_internal = "0.0.0.0:9023"
network = "testnet"
database_url = "postgresql://key_service_staging:${DB_PASSWORD}@${DB_HOST}:5432/key_service_staging"
kms_key_alias = "alias/options-staging-curator-keys"
```

Secrets TOML (`options/<env>/key-service`): the internal caller tokens. No Sui key of its own — it manufactures them. KMS access is via the instance role, not secrets.

**Backup note:** the `curator_keys` table is the only place hosted curator keys exist. It rides the shared RDS instance; confirm automated snapshots cover it before first real deploy, and remember the prod-DB-provisioning gap (prod DBs are hand-provisioned — `key_service_prod` must be created via `wipe-provision-db.sh` before the service's first prod deploy).

## 8. Alert ids

`key-service-sign-refused` (template mismatch — could be a gateway bug or an attack), `key-service-kms-error`. Not tx alerts (the service never submits), but same `alert_id` mechanism.
