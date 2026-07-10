# Security Notes & Audit Checklist

Threat-model reference for auditing the three programs in this workspace.
Structured around the Phase 4 hardening items in
`docs/solana/solana-port-plan.md` §8. Each item states where the defense
lives and which test exercises it.

## Audit packaging

Programs are designed for **staged, independent audits**
(plan §2): `options_core` has zero knowledge of the other two;
`auction_venue`'s generic machinery has zero dependencies (its option
adapters CPI only core's `write_collateralized` surface); `options_vault`
CPIs both. The only shared code is `crates/options_math` (~150 lines of
pure functions, golden-vectored against the Sui Move tests).

## Cross-program surfaces

| Threat | Defense | Test |
|---|---|---|
| Fake-program substitution | Every cross-program executable account is a typed `Program<'info, T>` (core/venue/token); every cross-program state account is either a typed `Account<'info, T>` (owner + discriminator via Anchor `Owner`) or an `UncheckedAccount` whose identity is pinned by an `address =` constraint against state the program itself wrote (`auction.bucket`, `vault.current_bucket`, `vault_position.position`) | Anchor-generated checks; exercised implicitly by every CPI test |
| Coupled-auction bypass (vault counter drift) | `Auction.settle_authority` (port of Move's `coupled` flag) requires the vault PDA as a CPI signer on every settle path; nobody else can produce that signature | `vault_full_round_lifecycle` (bypass block), `coupled_auction_requires_settle_authority` |
| CPI return-data spoofing | Not used. Post-CPI facts are read from accounts the callee mutated (position data re-deserialized with owner+discriminator checks; token balances via `reload()`) | `settle_rfq` position verification in `vault_full_round_lifecycle` |
| Oracle account forgery | `PriceUpdateV2` parse checks owner == Pyth receiver program, the 8-byte account discriminator, `VerificationLevel::Full`, pinned feed id, staleness, positivity, confidence ratio | `select_bucket_enforces_strike_band_and_freshness` (stale + wrong-feed rejections) |

## Quote verification (core)

| Threat | Defense | Test |
|---|---|---|
| Missing / wrong-program verify ix | `load_instruction_at_checked` + program-id pin | `missing_sig_instruction_fails`, `sig_index_pointing_at_non_precompile_fails` |
| Multi-signature header smuggling | `num_signatures == 1` enforced | `multi_signature_precompile_header_rejected` |
| Cross-instruction offset games | All three `*_instruction_index` fields must be `u16::MAX` (self-contained data only) | covered by construction in the above |
| Wrong signer / tampered message | Verified pubkey must equal the MM account's registered key; verified message must equal the Borsh canonical quote bytes | `wrong_signing_key_fails`, `tampered_quote_fails` |
| Nonce replay | `NonceRecord` PDA `init` fails on re-use before the handler runs | `nonce_replay_fails` |

## Token/mint integrity

- **Bucket isolation** (Sui's type-level guarantee, now runtime): every
  mint/burn path constrains `token::mint = bucket.call_mint` /
  `bucket.put_mint`, and only the bucket PDA holds mint authority.
  Supply == outstanding options is asserted across the lifecycle tests.
- **ATA verification**: bid refunds and expired-recovery refunds go only
  to the derived ATA of the recorded bidder; auction outputs go only to
  the exact `proceeds_token` / `refund_token` accounts fixed at creation.
- **Classic SPL only**: every mint flows through `Program<'info, Token>`;
  Token-2022 transfer hooks/fees would break amount-in == amount-credited
  invariants and are intentionally unsupported (plan §3).

## Account lifecycle (rent/close)

- `Position` closes to the redeemer; `NonceRecord` prunes to any caller
  after expiry (incentivized cleanup); auction + escrow vaults close to
  the creator at settle; `VaultPosition` closes to the cranker (including
  the manual close on the no-winner path, which wipes the discriminator
  before draining lamports); receipts close to their owners on claim.
  Exercised by the lifecycle tests in each suite.

## Known issues (accepted, inherited from the Move contracts)

1. **Call-bucket settlement rounding deficit.** Exercises pay
   `round_half_up(n × strike)` in; redemptions pay
   `round_half_up(exercised_range × strike)` out. Sums of half-up
   roundings over different partitions of the same range can differ, so
   with fractional strikes a bucket can end short by strictly less than
   one settlement smallest-unit per position — the final redeemer of a
   fully-drained bucket can fail for dust. Identical behavior exists in
   `bucket.move` (a `balance.split` abort). Bounded and pinned by
   `call_bucket_settlement_deficit_bounded_by_rounding`; the put side is
   immune by its ceil-in/floor-out design
   (`put_bucket_solvency_under_random_sequences`). Operational
   mitigation, as on Sui: choose strike scales so slice sizes × strike
   are integral, or top up dust before cleanup.
2. **Outbid-refund freeze grief.** Bid refunds push to the outbid
   bidder's ATA. A settlement mint with a freeze authority (e.g. USDC)
   could freeze that ATA and block further bids; the griefing bidder then
   wins at their own price above the reserve. Accepted for v1 (bidders
   are known MMs); the pull-refund escrow pattern is the documented
   fallback (plan §4.4).
3. **Keeper discretion inside oracle bands** is bounded, not eliminated —
   `min_reserve_premium_bps` is the true per-slice loss bound (same trust
   model as the Sui vault; see the keeper README there).

## Deliberate interface properties

- `write_collateralized` (call + put) is safe to expose permissionlessly:
  the caller fully collateralizes every option unit minted and holds both
  sides until they part with the coins. This is the venue's entire CPI
  surface into core.
- `deposit_protocol_fee` is permissionless by design (paying the treasury
  is harmless); the venue routes fees via direct SPL transfer to the
  treasury ATA, verified by owner.
- `payer` vs `writer`/`creator` splits exist because PDAs owned by another
  program cannot fund system-program account creation under CPI; the
  authority checks are unaffected.
