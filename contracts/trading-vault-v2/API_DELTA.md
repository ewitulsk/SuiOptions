# vault_v2 vs trading_vault (v1): API delta for porting code

Package `vault_v2` at `contracts/trading-vault-v2` replaces `trading_vault`
(`contracts/trading-vault`, kept in-tree as read-only reference). Module names
are unchanged (`vault`, `vault_mm`, `registry`, `price`, `events`, `errors`)
plus new modules `capital`, `fees`, `vault_position`. Dependency line:
`vault_v2 = { local = "../trading-vault-v2" }`; source refs `trading_vault::X`
→ `vault_v2::X`.

UNCHANGED (byte-compatible semantics): `price::attest` and the whole price
module; `registry` allowlists/pins and existing getters/setters;
`vault::begin_session` / `begin_quote_session` / `begin_crank_session`
signatures; `Session` take/put/put_position/take_position/receive_*/end_session;
`Appraisal` legs (`begin_appraisal`, `appraise_balance`, `record_position_value`,
`record_external_equity`, `check_attestation`, `crank_appraisal`);
external-account surface (except `release_external`, below); closure
(`initiate_close`, `finalize_close`); error codes 70..113.

CHANGES:

1. `create_vault<T>(cfg, wl, lockup_ms, curator_fee_bps, unwind_grace_ms, ctx)`
   → `create_vault<T>(cfg, wl, lockup_ms, curator_fee_bps, unwind_grace_ms,
   structure_code: u8, senior_hurdle_bps_annual: u64, target_junior_bps: u64,
   maintenance_junior_bps: u64, upside_code: u8, residual_participation_bps: u64,
   total_return_cap_bps: u64, terms_version: u64, spec_hash: vector<u8>,
   clock: &Clock, ctx)`. Untranched vault: structure_code 0 and all six tranche
   params 0.
2. `deposit<T>(vault, cfg, wl, appraisal, funds, att, clock, ctx)` →
   adds `tranche_code: u8` (0 untranched / 1 senior / 2 junior) before `clock`,
   and now RETURNS a `vault_position::VaultPosition` (key+store) that the caller
   must transfer/keep.
3. `deposit_as_curator` → REMOVED. Curator self-stake is now
   `deposit_into_commitment<T>(vault, cfg, wl, cap, appraisal, funds, att,
   clock, ctx)` (no return; mints into the in-vault escrowed commitment
   position).
4. `request_withdraw<P>(vault, shares, clock, ctx)` →
   `request_withdraw<P>(vault, position: VaultPosition, clock, ctx)` — consumes
   a whole position object. Partial exit = `vault_position::split` first.
   `request_withdraw_as_curator` → REMOVED (use
   `release_commitment(vault, cap, cfg, appraisal, shares, clock, ctx):
   VaultPosition` — shares==0 releases all — then request_withdraw).
5. `enqueue_closed_stake` → REMOVED. Closed vaults: permissionless
   `snapshot_settlement(vault, cfg, appraisal, clock)` freezes entitlements;
   then `redeem_settled_position<T>(vault, cfg, treasury, position, ctx)`,
   `settle_queued_request<T>(vault, cfg, treasury, global_seq, ctx)`,
   `withdraw_commitment_settled(vault, cap): VaultPosition`,
   `claim_settlement_curator_fees<T>(vault, cap, ctx)`.
6. `begin_fulfillment` and `begin_force_session` now take `&mut TradingVault`.
7. `release_external<T>(vault, cap, appraisal, amount, clock, ctx)` →
   `release_external<T>(vault, cap, cfg, appraisal, amount, clock, ctx): u128`
   (added cfg; returns consumed NAV; aborts 124 when risk-off).
8. `stake_of` / `curator_stake_of` → REMOVED. Claims are wallet-held
   `VaultPosition` NFTs (getters in `vault_v2::vault_position`); the curator's
   escrow is read via `vault::commitment_of(vault, cap_id): (bool, shares,
   basis, generation)`.
9. Queue getters: `queue_head`/`queue_tail` → `lane_bounds(vault, lane_u8)`
   (lane 0 senior / 1 junior; untranched uses lane 1), `lane_entry(vault, lane,
   idx) -> global_seq`, `has_request(vault, global_seq)`;
   `queue_request(vault, global_seq)` now returns `(position_id, recipient,
   tranche_code, generation, shares, basis, payout_asset, requested_at_ms,
   lane_code)`. `amend_payout_asset<P>(vault, global_seq, ctx)`.
10. RISK-OFF GATING (§8.4b): every vault syncs a capital risk state at each
    consumed-appraisal mutation. Risk-off = tranched vault in
    CoverageBreach/Impaired/ResetPending OR curator commitment below the
    protocol floor (default 2% of NAV, marked). While risk-off:
    `begin_session` opens FORCED (take aborts code 91), `begin_quote_session`
    aborts 124, `release_external` aborts 124, `vault_mm::release` aborts 124.
    TESTS that need take-capable sessions must either fund the commitment
    (`deposit_into_commitment` ≥ floor) or disable enforcement:
    `registry::set_enforce_curator_share(&admin_cap, &mut cfg, false)`.
11. New: `crank_capital(vault, cfg, appraisal, clock)` (mutable capital sync),
    `propose_junior_reset(vault, cfg, appraisal, clock)`,
    `execute_junior_reset<T>(...): VaultPosition`, `burn_wiped_position`,
    `vault_position::split/merge`, `vault::book(vault): &TrancheBook`,
    `vault::capital_structure(vault)`, `capital::*` getters.
12. New error codes 120..136 (see `sources/errors.move`).
13. Events: see `sources/events.move` — `Deposited` gained
    position_id/tranche/generation, `WithdrawRequested`/`WithdrawFulfilled`
    keyed by global_seq + lane, new CapitalSynced/RiskStateChanged/
    JuniorReset*/Settlement*/Position* events. Test helpers that
    pattern-matched v1 event shapes must update.
14. In vault_v2 tests, `vault_v2::test_helpers` provides `fund_commitment`,
    `deposit_usdc(scenario, who, amount, tranche_code, clock)`,
    `new_default_vault(scenario, clock)`, `new_tranched_vault(...)`,
    `request_withdraw_all`, `run_fulfillment`, `crank_capital`.
