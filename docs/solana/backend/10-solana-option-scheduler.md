# solana-option-scheduler (`services/solana-option-scheduler`)

Bucket-roll + vault-provisioning bot. **Standalone workspace**. Holds the
**admin keypair** (`options_core` Config.admin / vault admin) — the only
privileged service, as on Sui (deployer/AdminCap holder).

## The big simplification

Sui rolls required: codegen OTW coin packages → in-process Move compile →
publish → harvest TreasuryCaps → create buckets + DeepBook pools. On Solana,
**`call_mint`/`put_mint`/`share_mint` are PDAs created by the program** — a
roll is just N `create_bucket(salt, expiry_ms, strike, strike_scale)`
instructions, and vault creation is one `create_vault(salt, config)`. No
codegen, no compiler, no cap harvesting, no pools (no order book yet).
`codegen.rs`, `coin_pkg`, Move.toml plumbing: all gone.

## What ports unchanged

- Tick loop (60 s) per `[[pairs]]`; **DB-first roll state** (never trust
  indexer freshness for dedup): `latest_active_expiry` from DB → `decide_tick`
  within `roll_threshold_ms` → cadence-derived next expiry → `claim_slot`
  (partial UNIQUE index on `(underlying, settlement, expiry_ms, product_type)`
  WHERE active — the hard dedup) → submit → `mark_submitted`.
- Strike grids: percent grid + vol-aware z-ladder (`strike_grid.rs` is pure
  math — ports verbatim), σ from solana-oracle realized vol with
  floor/ceiling/fallback, spot as USD/USD cross from oracle-client.
- Reconciler (30 s): confirm landed rolls from indexer `buckets`, supersede
  fully-invalidated families, expire `needs_reconciliation` rows past the
  sequence anchor + safety margin. **Uses `finalizedOnly: true`** event/bucket
  reads for confirmation (roll confirmation is fold-into-own-state — the
  two-tier rule from the indexer guide).
- Error classes: DefinitelyNotSent (build/sign/simulate-reject → delete
  pending, retry next tick) vs Ambiguous (send timeout → needs_reconciliation).
  On Solana, "Ambiguous" additionally resolves definitively via
  `getSignatureStatuses` on the recorded signature — the reconciler checks
  that first, then falls back to the indexer-anchor scan.
- Vault-ensure (hourly): retire paused, match existing by
  (mints, round_ms, !paused), else `create_vault`. The multi-step
  `coin_published` crash-recovery state collapses to a single-tx create —
  vault states become `pending|confirmed|failed|retired`.
- Alerts: `error!(alert_id = "tx-failed-solana-option-scheduler")` and
  `…-vault`.

## Salts, atomicity, and batching

- `salt` (bucket/vault PDA seed) = deterministic hash of
  `(underlying_mint, settlement_mint, expiry_ms, strike, strike_scale,
  product_type)` truncated to u64 — re-runs derive the same PDA, so a
  double-submit collides on-chain (`already in use`) instead of duplicating;
  classified Benign. This complements the DB unique index.
- One transaction per `create_bucket` (Anchor init of bucket + mint + two
  vault ATAs is compute/size-heavy); a roll of N strikes = N txs submitted
  sequentially with per-tx confirm. `bucket_ids` (= derived PDAs) recorded on
  the roll row up front; partial-failure just resumes (same salt → idempotent).
- **Strike-scale rule** (dust mitigation from SECURITY.md): grids choose
  `strike_scale` such that `strike × min_slice` is integral in settlement
  units, mirroring the existing auto-derive but with the extra integrality
  check; unit-tested.

## DB

`solana_scheduler_<env>`; diesel embedded migrations. `scheduler_rolls`
(minus tx_digest → `signature`, `bucket_ids` = base58 PDAs) and
`scheduler_vaults` (drops share-coin columns; keeps
`(underlying, settlement, round_ms)` active-unique).

## Config / secrets

- `indexer_graphql_url`, `tick_secs`, `roll_threshold_ms`,
  `scheduler_database_url` (`postgresql://solana_scheduler_<env>:…`),
  reconciler keys, `[[pairs]]` (symbols resolved via solana-token-info
  catalog: mint + decimals + feed id), `[vault_template]` — vault config
  fields map 1:1 to `VaultConfig` (feed ids + decimals pinned from the
  catalog).
- Boot assertion: `signer pubkey == program_info.admin` (parallel of
  signer==deployer).
- CLI: `--token-info-url`, `--oracle-url`, `--secrets`, `--network`.
- Secrets `options/<env>/solana-scheduler`: `[solana]` admin keypair + rpc
  override. Ops port `health_addr 0.0.0.0:8087`.

## Verification

- Unit: strike grid integrality, salt determinism, decide_tick/cadence,
  claim-slot dedup (needs a DB — use the existing scheduler test pattern),
  reconciler state transitions with fixture indexer responses.
- litesvm integration: create_bucket + create_vault happy path with the real
  program crates (same harness as solana-keeper's).
