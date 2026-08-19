# Trading Vault v2 — Operations and Incident Runbooks

Required by the overhaul plan §9.4. Alert owner for every `alert_id` below:
Evan (on-call), via the standard Grafana contact points. On-chain evidence
(tx digests + `CapitalSynced`/`RiskStateChanged` events) is required before
any public status communication.

## Keeper duties (cadence obligations)

| Duty | Cadence | Why |
| --- | --- | --- |
| `crank_capital` per vault | every `mark_refresh_interval_ms` | Correctness bound: hurdle accrual caps elapsed time at 2 years (spec §2) — cadence must stay ≪ cap. Keeper config validation rejects `mark_refresh_interval_ms ≥ 1/1000` of the cap; alert `tv-accrual-cadence` fires when a vault's last consumed appraisal ages past 1/100 of the cap. |
| Lane fulfillment crank | on pending requests | "Lowest global seq among payable lane heads"; a blocked junior lane must never stop senior draining. |
| Risk-state transition watch | every crank | `RiskStateChanged` → alerts below. |
| Terminal settlement snapshot | once per vault on `Closed` | Permissionless `snapshot_settlement`; a Closed-but-unsettled vault is an incident after 1h (`tv-settlement-missing`). A settled vault needs zero further cranking. |

## Incidents

### Coverage breach (`alert_id = "tv-coverage-breach"`, warning)
Junior buffer < maintenance. Expected contract behavior: junior lane paused,
senior flowing, curator sessions forced (unwind-only), quote sessions/mm
release/external release abort. Response: verify the triggering
`CapitalSynced` NAV against independent marks; notify the curator (cure paths:
junior deposits, realized gains, de-risking); confirm mm-bot stopped quoting
the vault (it must pre-check risk state). No admin action required — the
state machine is self-curing on marks.

### Impairment (`alert_id = "tv-impaired"`, critical)
`NAV < senior claim`. Junior is wiped at marks. Verify marks FIRST (a bad
oracle print can cause a spurious impairment; recovery on the next good
appraisal auto-clears). If real: curator unwinds; watch for
`JuniorResetProposed`.

### Reset proposed (`alert_id = "tv-reset-proposed"`, critical — user-facing)
Anyone proposed a junior reset. Surface prominently (UI banner + comms):
old generation, executable time, recorded quote, and the caveat that the
binding deposit is recomputed at execution. If impairment cures, expect
`JuniorResetCancelled` — verify the flag cleared. On execution
(`JuniorResetExecuted`): confirm generation bump, new junior position, and
that the curator re-funds a new-generation commitment before expecting
risk-on.

### Curator commitment breach (`alert_id = "tv-commitment-breach"`, warning)
Marked escrow value below the protocol floor (or rotation/reset pessimistic
flag). Deployment halts until `deposit_into_commitment` cures it. If this is
the desk vault, the mm-bot's quoting stops — treat as a trading incident.

### Stale appraisal / wedged NAV
Symptom: deposits/fulfillments abort 82/83. Usual causes: a held asset lost
its oracle pin/allowlist, an adapter position needs its appraisal leg, or
live external exposure lacks an equity post. Fix the pricing path; NEVER
work around by delisting the appraisal requirement. Appraisals must keep
working in every risk state or nothing can cure.

### Insufficient withdrawal liquidity / blocked head
Head not payable in its requested asset: after `unwind_grace_ms` the crank
pays the accounting asset (grace fallback) and permissionless force sessions
unlock (aged on the OLDEST head across lanes — a blocked junior lane still
counts). Recipients can `amend_payout_asset`. Escalate to curator unwind if
free balances cannot fund the head.

### Blocked junior lane
Not an incident by itself — it is the designed §3.6 behavior during
breach/impairment. Incident only if the lane stays blocked after the state
returns Healthy (would indicate a fulfillment bug: check
`is_junior_blocked` vs lane head).

### Terminal close and settlement
`initiate_close` → unwind (force sessions available) → `finalize_close`
(needs zero positions, zero external exposure, accounting asset only) →
`snapshot_settlement` → queued requests settled permissionlessly
(`settle_queued_request`), wallet positions self-serve. Report unredeemed
claim totals per vault (indexer) indefinitely — "fully closed" means
settled, not zero shares.

## Standing conventions

- Every service tx-submission failure logs `error!(alert_id = "tx-failed-…")`
  at the service handler, benign race-losses suppressed (repo convention).
- v2 abort codes: 70–113 unchanged from v1; 120–136 new (see
  `contracts/trading-vault-v2/sources/errors.move`). Benign-abort
  classification for cranks: 78 (insufficient balance), 82/83 (appraisal
  raced), 86 (position raced), plus 124 (risk-off) and 136 (queue settled)
  for quote/settlement paths.
