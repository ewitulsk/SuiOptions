# Trading Vault v2 — Parameter and Governance Registry

Companion to [spec.md](spec.md) (terms version 1); required by the overhaul
plan §9.2. A contract upgrade must not retroactively change immutable vault
economics; any future economic change gets a new terms version with explicit
holder migration/opt-in rules.

## Immutable per-vault (fixed at `create_vault`, forever)

| Field | Units | Range enforced | Notes |
| --- | --- | --- | --- |
| `accounting_asset` | type | — | Unit of account for NAV/basis/fees/budgets |
| `capital` (structure) | enum | Untranched or SeniorJunior | Cannot be toggled after assets exist (plan §3.2) |
| `senior_hurdle_bps_annual` | bps/yr | ≤ `max_senior_hurdle_bps` | Simple cumulative accrual (spec §2) |
| `target_junior_bps` | bps | ≥ `min_target_junior_bps`, < 10⁴ | Senior-issuance gate (spec §4) |
| `maintenance_junior_bps` | bps | ≥ `min_maintenance_junior_bps`, ≤ target | Breach trigger (spec §7) |
| `upside` mode + `residual_participation_bps` (+ `total_return_cap_bps`) | enum, bps | participation ≤ 10⁴; cap ≥ 10⁴ (capped mode) | Spec §3 |
| `terms_version`, `spec_hash` | u64, bytes | — | Emitted in `VaultCreated` |

## Curator-managed per-vault (current cap holder)

| Field | Units | Range | Change path |
| --- | --- | --- | --- |
| `deposit_assets` | type set | size ≤ `max_deposit_assets`; accounting asset always present | `add/remove_deposit_asset` |
| `lockup_ms`, `curator_fee_bps` (≤ `max_curator_fee_bps`), `unwind_grace_ms` | ms / bps / ms | fee capped at creation | Fixed at creation |
| `entry_haircut_bps`, `exit_haircut_bps` | bps | ≤ 500 | `set_haircuts` |
| `quote_adapters` | type set | protocol-allowlisted only | `add/remove_quote_adapter` |
| `deposits_paused`, `mm_release_enabled` | bool | — | curator toggles |

## Protocol-governed (AdminCap on `VaultProtocolConfig`)

| Field | Default | Units | Bound | Who |
| --- | --- | --- | --- | --- |
| `min_curator_share_bps` (legacy floor knob) | 500 | bps | ≤ 10⁴ | Admin |
| `enforce_curator_share` | true | bool | — | Admin (kill switch for commitment tests) |
| `max_curator_fee_bps` | 3000 | bps | ≤ 10⁴ | Admin |
| `protocol_fee_bps` (share OF curator fee) | 1000 | bps | ≤ 10⁴ | Admin |
| `max_price_age_ms` | 60,000 | ms | > 0 | Admin |
| `max_deposit_assets` | 8 | count | > 0 | Admin |
| `paused` (protocol deposit pause; never blocks exits) | false | bool | — | Admin |
| `registrar_pubkey` | empty (attested path disabled) | ed25519 | 0 or 32 bytes | Admin |
| `max_senior_hurdle_bps` | 2000 | bps/yr | ≤ 10⁴ | Admin |
| `min_target_junior_bps` | 1000 | bps | 0 < x < 10⁴ | Admin |
| `min_maintenance_junior_bps` | 500 | bps | 0 < x < 10⁴ | Admin |
| `min_curator_commitment_bps` (marked junior value / NAV) | 200 | bps | ≤ 10⁴ | Admin |

Governance changes apply prospectively at the next capital sync; they never
rewrite a vault's immutable creation terms.

## Protocol constants (immutable in code; changing any requires a new terms version)

| Constant | Value | Meaning |
| --- | --- | --- |
| `SHARE_OFFSET` | 1,000,000 | Virtual-offset shares per tranche |
| `MAX_HAIRCUT_BPS` | 500 | Cap on entry/exit haircuts |
| `MS_PER_YEAR` | 31,536,000,000 | Hurdle time basis (365d) |
| `ACCRUAL_CAP_MS` | 63,072,000,000 (2y) | Accrual overflow bound (spec §2) |
| `RESET_SEASONING_MS` | 604,800,000 (7d) | Reset seasoning AND notice minimum |
| `RELEASE_WINDOW_MS` | 86,400,000 | External-account rate-limit window |
| `ATTESTED_MAX_BUDGET_BPS` / `ATTESTED_MAX_DAILY_RELEASE_BPS` | 2000 / 1000 | Self-serve external registration caps |
| `EXTERNAL_REG_DOMAIN` | `tv_external_reg_v1` | Byte-identical to v1 (FROST ceremonies survive) |

## Safe-range guidance (product, not consensus)

- `target_junior_bps` 2000–3000 is a product starting point, not a safety
  conclusion; calibrate to strategy drawdown, liquidation horizon, oracle
  conservatism, and off-chain exposure (plan §8.4).
- Keeper capital-crank cadence must satisfy
  `mark_refresh_interval_ms ≪ ACCRUAL_CAP_MS` (see runbooks; enforced as an
  off-chain config validation in the keeper).
