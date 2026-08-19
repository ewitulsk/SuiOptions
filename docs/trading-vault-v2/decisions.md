# Trading Vault v2 — Decision Records and Release Gate

Required by the overhaul plan §9.5. Approver: Evan Witulski. Date: 2026-08-19.
Spec: terms version 1 ([spec.md](spec.md)). Each record states the selected
rule, the alternatives, and the consequences; full analysis lives in
[trading-vault-overhaul-plan.md](../trading-vault-overhaul-plan.md) (Revision
3), cited per record.

| # | Decision | Selected rule | Alternatives rejected | Consequences |
| --- | --- | --- | --- | --- |
| 1 | NFT standard & transfer policy (plan §2.1, §8.1) | Plain Sui object `key + store` with `Display`; **unconditional transferability for every wallet-held position**; whitelist gates primary issuance only | `Coin<VaultShare>` (destroys per-lot basis/lock); controlled-transfer/whitelisted-transfer variants (unenforceable against `store`) | Secondary buyers inherit basis/fee liability; **non-whitelisted parties can buy and redeem** — accepted and disclosed (compliance consequence recorded; counsel sign-off tracked on SO-417) |
| 2 | Curator commitment enforcement (plan §2.2, §8.6) | Commitment position ESCROWED inside the vault, keyed by current cap; measured as marked junior value ≥ protocol bps of NAV; breach applies §8.4b gates minus junior-lane block | Non-transferable curator NFT (unenforceable); nominal share-percentage floor (meaningless across tranches) | Fee mints have a guaranteed credit target; rotation/reset force re-funding before risk-on |
| 3 | Hurdle accrual (plan §8.2) | Simple, continuous, **cumulative through impairment**, 365d year, 2-year elapsed cap as pure overflow bound | Periodic compounding (state/rounding complexity); epoch hurdle (changes synchronous deposit model); non-cumulative-in-impairment (weaker senior product — possible future creation-time enum) | Keeper cadence becomes a correctness obligation (runbooks); arrears absorb recovery until cured |
| 4 | Senior upside modes (plan §8.3) | Three immutable creation-time modes through one general waterfall: PreferredOnly (default), CappedParticipating (participation bps + total-return cap bps), UncappedParticipating | Bare `capped: bool`; mutable participation | Mode-qualified invariants and disclosures; UI must distinguish claim accrual from participation |
| 5 | Coverage thresholds & risk-off gate set (plan §8.4, §8.4b) | Two thresholds on appraised NAV (creator target ≥ floor gates senior issuance; immutable maintenance triggers breach); mechanical gate set: curator sessions forced, quote sessions/mm release/external release abort; force/crank/inbound/appraisals untouched | Issuance-only minimum (protection evaporates); continuous single threshold (boundary flapping); haircut-based eligible-asset test (adapter-specific, deferred) | "Nothing leaves free balances except fulfillment" is the auditable invariant |
| 6 | Queue lanes (plan §3.6) | Per-tranche FIFO lanes under one global sequence; pay lowest global seq among payable heads; class-blocked junior never stalls senior; force-unwind ages on oldest head overall | Single strict-head FIFO (liveness failure: blocked junior head freezes senior) | Reduces exactly to v1 global FIFO when unblocked; lane-aware keeper/indexer |
| 7 | Junior reset (plan §8.5) | Generational claims; objective eligibility; 7d seasoning + 7d notice; recovery auto-cancels; atomic execution with fresh appraisal + minimum deposit recomputed at execution, rounded up; genesis mint to recapitalizer; senior claim not written down; no discretionary seizure | Burning wallet NFTs (impossible); admin wipe (governance risk); legacy recovery warrant (a fourth tranche — out of scope) | Old generations are permanent zero-value claims with a burn path; curator must re-commit in the new generation |
| 8 | Terminal settlement (plan §8.7) | One-time permissionless snapshot freezing per-share entitlements, senior first, pro rata within tranche; positions redeem against the pool forever; "fully closed" = settled | `enqueue_closed_stake` sweep (unreachable wallet NFTs); forced expiry of claims | Vault persists as claim-only shell; indexer reports unredeemed totals; curator settlement fees accrue as cash (no post-snapshot share mints) |
| 9 | Fee timing (plan §8.8, §3.5) | Exit crystallization on per-lot basis; same-tranche fee mints credited to escrow, senior mints credit the claim; settlement fees as cash claim | Pooled high-water mark / equalization (needed only for fungible coins; second phase); periodic crystallization (cannot iterate wallet NFTs) | Transfer is never a fee event; per-lot loss recovery uncharged |
| 10 | Release shape (plan §5, §7) | One complete v2 release; new package `vault_v2` replacing `trading_vault`; v1 kept in-tree as read-only reference until post-rollout removal; no migration machinery (no live vaults) | Phased rollout (Sui cannot add struct fields; forces a second republish with live users); in-place layout change (impossible) | Single audit scope over the full economic surface; adapters repointed in the same release |

## Release gate (fail-closed checklist)

1. ☑ Normative spec + parameter registry approved and versioned (terms v1:
   spec.md, parameters.md).
2. ☑ Move unit/property tests cite spec cases (spec.md §9 anchor table).
3. ☐ SDK/indexer/UI behavior checked against the §7 action matrix — the
   SO-418 off-chain release (v2 smoke = acceptance artifact).
4. ☑ Disclosures and runbooks published (disclosures.md, runbooks.md).
5. ☐ Audit scope citing terms version 1; report must show no undocumented
   economic behavior. **Not yet performed — blocks mainnet, not staging.**
6. ☐ Deployment manifest records package id, terms_version, spec hash, audit
   report (deployment-manager change in SO-418).
