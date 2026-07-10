# Solana Port Plan

Port of the SuiOptions smart contracts (`contracts/`) from Sui Move to Solana,
including the test suite. Session-key functionality (`session_*` modules) is
**explicitly out of scope** and will not be ported.

The port is organized as **three independently auditable, independently
releasable programs** so contracts can be audited and shipped in pieces as the
product gains market fit:

1. `options_core` — the option protocol (calls + puts). Audit package 1.
2. `auction_venue` — the on-chain RFQ / auction venue. Audit package 2.
3. `options_vault` — the covered-call vault. Audit package 3.

Dependency arrows point strictly downward (vault → venue → core). **No auction
mechanism is required for core to work**: core is fully functional standalone
via the quote-based `execute_write` MM flow and the `write_collateralized`
self-write primitive; `exercise` / `redeem` / `burn` never know how a write
originated. The venue is strictly additive, and the vault is the only
component that requires the venue (its design premise is on-chain price
discovery per round).

---

## 1. Scope: what is being ported

The contracts have evolved well past the original v0.1 spec
(`options-protocol-spec.md`). The feature inventory:

| Move module | Feature | Delta vs. original spec |
|---|---|---|
| `bucket.move` | Covered-call buckets, FIFO cursor, write/exercise/redeem/burn-expired/cleanup | u128 strike + u8 `strike_scale` with round-half-up; admin invalidate/revalidate; public `write_collateralized` self-write primitive; fungible per-bucket option coin |
| `put_bucket.move` | Cash-secured puts (mirrored legs) | ceil-in/floor-out rounding with documented solvency proof; `total_redeemed` cleanup gate; dust sweep to admin |
| `account.move` / `quote.move` | MM accounts, multi-scheme signed quotes (Ed25519, secp256k1, secp256r1), nonce replay protection, permissionless nonce pruning | Multi-scheme signing added post-spec |
| `rfq.move` / `rfq_put.move` | On-chain ascending premium auctions: escrowed bids, reserve floor, min-increment, anti-snipe extension, permissionless settle, expired-bucket recovery, vault-coupled variant | Post-spec |
| `swap_auction.move` | Pyth-bounded reverse auction converting vault settlement proceeds → underlying (replaced DeepBook swap) | Post-spec |
| `oracle.move` | Pyth U/S cross with feed pinning, staleness, positivity, confidence checks; exact integer strike/spot comparison at `ORACLE_PRICE_SCALE = 12` | Post-spec |
| `vault.move` | Ribbon-style weekly-round covered-call vault: pps accounting mirroring `vault-sim`, deposit/withdraw receipt queues, profitable-round-only fees (mgmt-first/perf-clamped), permissionless lifecycle cranks, Pyth-banded bucket selection, deferred config updates, oracle-feed escape hatch | Post-spec |
| `admin.move` / `treasury.move` / `events.move` / `errors.move` / `position.move` | Config (fee cap 1000 bps), asset-agnostic treasury, 53 events, 59 error codes, transferable Position | Mostly per spec |

**Dropped**: `session_account`, `session_bucket`, `session_put_bucket`,
`session_vault`, `session_deepbook`, their 4 test files, the vendored DeepBook
deps, and the session hooks inside `account.move` (`new_with_owner`,
`uid`/`uid_mut`, the `AccountPosition*/AccountObject*` events).

**Test surface**: 142 `#[test]` functions across 12 non-session test files
(~5,300 lines) plus `test_helpers.move`, all ported (re-homed per program).

---

## 2. Program topology

```
┌─────────────────────┐         ┌─────────────────────┐
│  options_vault      │ ──CPI──►│  auction_venue      │
│  (audit package 3)  │         │  (audit package 2)  │
│                     │         │                     │
│                     │ ──CPI──►│  generic auction    │──┐
└─────────────────────┘         │  core: NO deps      │  │ CPI (option-settle
          │                     │  ─────────────────  │  │  adapter only)
          │ CPI                 │  option adapters    │──┤
          ▼                     └─────────────────────┘  ▼
┌──────────────────────────────────────────────────────────┐
│  options_core  (audit package 1)                         │
│  call buckets + put buckets + positions + quotes/MM      │
│  accounts + admin/config + treasury                      │
│  knows NOTHING about venue or vault                      │
└──────────────────────────────────────────────────────────┘
          shared: options_math crate (no-std, golden-vectored)
```

- Core compiles, deploys, tests, and audits with zero knowledge of the other
  two programs.
- The venue's auction machinery has zero dependencies on anything; only its
  thin option-settle adapter CPIs core. In pure-swap mode the venue is a
  usable generic token-auction product with no core deployment at all.
- The vault CPIs both and holds no logic either of them needs.
- The only shared code is the `options_math` crate (§6) — everything else
  crosses boundaries via CPI interfaces and account schemas only.

If a literally core-free venue binary is ever required, the option-settle
adapter can be split into a fourth micro-program; not recommended (more
deploys, same audit surface), but it is a clean fallback.

---

## 3. Stack decisions

- **Framework: Anchor** (latest, with `anchor-spl`). The contracts are
  check-heavy, event-heavy, multi-account; Anchor's constraint system replaces
  Move's type-level guarantees with declarative runtime checks, and its IDL
  feeds the indexer.
- **Layout**: one Anchor workspace `solana-contracts/` with
  `programs/options_core`, `programs/auction_venue`, `programs/options_vault`,
  `crates/options_math`, `tests/` (Rust). Sui `contracts/` stays untouched
  alongside.
- **Tokens: classic SPL Token only for v1.** Token-2022 transfer hooks/fees
  would silently break bucket-balance and vault-pps invariants. Gate all mints
  with `token::token_program = token::ID`.
- **Timestamps: keep every field in ms** for parity with the off-chain stack
  and quote format; read `Clock::unix_timestamp` × 1000. One-second
  granularity is immaterial at these horizons.
- **Math**: `u128` is native Rust. The two u256 spots (oracle cross, vault
  strike-band cross-multiplication) use a uint crate (`ruint` or
  `primitive-types::U256`).
- **Events**: Anchor `emit_cpi!` (log-truncation-proof, indexer-friendly). All
  49 non-session events port 1:1. Error enum ports 1:1 minus session codes.

---

## 4. Core representation mapping (Sui → Solana)

### 4.1 Objects → PDAs

| Sui | Solana |
|---|---|
| `Bucket<U,S,C>` shared object | `Bucket` PDA: seeds `["bucket", underlying_mint, settlement_mint, expiry_le, strike_le, strike_scale, salt]`; stores mint pubkeys + `call_mint` + cursor fields. Two PDA-owned token vaults (underlying, settlement). |
| `PutBucket<U,S,P>` | Same shape, `["put_bucket", …]`, plus `total_redeemed`. |
| `Account` shared object | `MmAccount` PDA `["account", owner]` (+ salt if needed): owner, signing_scheme, signing_pubkey. Balances = PDA-owned token accounts per mint — replaces dynamic fields. |
| `Treasury` | `Treasury` PDA + ATAs per mint. |
| `ProtocolConfig` + `AdminCap` | `Config` PDA `["config"]`: `admin: Pubkey`, `fee_bps`, `protocol_id` (config PDA address as domain separator). AdminCap transferability → `set_admin` instruction. |
| `Position` owned object (transferable) | `Position` PDA `["position", bucket, range_start_le]` (range_start unique per bucket — the cursor is monotonic): `owner, bucket, range_start, range_end`. `transfer_position` ix preserves transferability. Closed at redeem, rent → redeemer. Plain owned record, **not** an NFT (see decision points). |
| `RfqAuction` / `PutRfqAuction` / `SwapAuction` shared objects | One generic `Auction` PDA (§5.2) + PDA-owned escrow token accounts. Closed at settle, rent → creator. |
| `Vault<U,S,V>` | `Vault` PDA + token vaults for `deployable`, `pending_deposits`, `proceeds_settlement`, `withdrawal_pool`, `claimable_shares`, `queued_withdraw_shares` (last two hold the vault's own share tokens). |
| `DepositReceipt` / `WithdrawReceipt` | Per-user receipt PDAs with `owner` field, closed on claim/complete (rent → user). |
| `Table<u64,u128> pps` | Per-round `RoundState` PDA `["round", vault, round_le]` created at finalize. |
| `ObjectTable positions` FIFO | `["vault_pos", vault, index_le]` PDA storing the core Position key + `positions_head/tail`; `crank_redeem` requires index == head. |

### 4.2 `Coin<Call>` per bucket → SPL mint per bucket (major simplification)

Sui's OTW restriction forced the entire per-roll codegen → compile → publish →
harvest-`TreasuryCap` pipeline (spec §3.4, commit `41d37f3`). On Solana,
`create_bucket` simply **creates a fresh SPL mint in the same transaction**:

- mint authority = the bucket PDA (only the program mints/burns — supply ==
  outstanding options, same invariant),
- freeze authority = none,
- decimals = underlying mint's decimals (display parity).

Bucket isolation degrades from a type-system guarantee to a runtime check
(`token_account.mint == bucket.call_mint` as an Anchor constraint on every
exercise/burn path) — standard Solana idiom, but an explicit audit-checklist
item. In exchange the options-scheduler loses its most fragile subsystem
entirely. Same for put mints and vault share mints (`VShare` → share mint with
vault PDA authority, replacing the share `TreasuryCap`).

### 4.3 Signed quotes → precompile + instruction introspection

- The execute-write transaction includes a **precompile instruction**
  (Ed25519Program / Secp256k1Program / the secp256r1 precompile — verify
  feature status on the target cluster) verifying `sig` over the canonical
  quote bytes against `account.signing_pubkey`.
- The program reads the **Instructions sysvar** and asserts the precompile ix
  precedes it with exactly the expected pubkey/message/signature.
- **Canonical bytes change from BCS to Borsh** (field order preserved:
  protocol_id, signer_account, signer_token_recipient, bucket, write_amount,
  premium, valid_until_ms, nonce; IDs/addresses become 32-byte Pubkeys).
  Golden-vector this against the quoting service / mm-bot signer.
- **Nonces**: consumed nonce = a `NonceRecord` PDA `["nonce", account,
  nonce_le]` created at execute (init-fails-if-exists **is** the replay
  check), storing `valid_until_ms`. `prune_nonce` closes it after expiry with
  rent to the caller — a better incentive than Sui's storage rebate.

### 4.4 Semantics needing explicit care

1. **Push refunds in auctions.** `rfq::bid` push-transfers the outbid escrow
   ("always succeeds on Sui"). On Solana: the `bid` ix takes the previous
   bidder's **ATA** (derivable from stored `best_bidder`), created
   idempotently with the new bidder as payer. Documented caveat: a settlement
   mint with a freeze authority (USDC) could freeze an outbid MM's ATA and
   wedge bidding; acceptable for v1 (bidders are known MMs), with the
   pull-refund escrow pattern (`["bid_refund", auction, bidder]` PDA + claim
   ix) as the fallback design.
2. **`transfer::public_transfer(coin, addr)` in settle paths** (net premium →
   seller, refunds) — same ATA treatment: settle ix receives recipient ATAs,
   verified against addresses stored in the auction, create-idempotent.
3. **Atomicity**: PTB flows become multi-instruction transactions. Nothing
   exceeds account-lock or tx-size limits. Write-lock contention on hot
   bucket/account PDAs replaces Sui shared-object consensus — same throughput
   caveat as spec §3.6.6.
4. **Pyth**: `pyth-solana-receiver-sdk` `PriceUpdateV2` accounts replace
   `PriceInfoObject`. `oracle.rs` ports `spot_cross` verbatim: feed-id
   pinning, staleness (future-skew tolerated), positivity, confidence-ratio
   cap, exponent-accumulator cross math at scale 12. Stays test-locked
   against `mm-bot/src/pricing.rs` vectors.
5. **`cleanup_bucket`**: close the bucket PDA + vaults (rent → admin), and
   transfer the call-mint authority to the admin — the exact analog of
   handing back the `TreasuryCap` (outstanding coins may exist; supply can't
   be forced to zero). Put-bucket dust sweep (`total_redeemed ==
   total_written` gate) ports as-is.
6. **Rounding math ports bit-for-bit** (see §6). These are the audit-critical
   lines; port them as a pure crate with the Move file open next to you.

---

## 5. The three programs

### 5.1 `options_core` — audit package 1

Call `Bucket`, `PutBucket`, `Position` (+ transfer), `MmAccount` + nonce PDAs
+ precompile quote verification, `Config`/admin, `Treasury`, all bucket-level
events/errors. Calls and puts stay in **one** program: they share quotes,
accounts, treasury, position type, fee logic, and half their math; splitting
doubles audit cost for zero isolation benefit, and they ship together.

Split-driven design points:

- **`write_collateralized` (call + put) is the official composability
  surface** (the Move doc-comment already frames it as "the primitive that
  lets anyone build a venue"). First-class CPI treatment: accepts a
  `position_owner: Pubkey` and a call/put token destination, pulls collateral
  from any funder token account (so a venue escrow PDA can fund via CPI
  signer), returns position key + range via CPI return data. It is exactly as
  safe as on Sui — fully collateralized, no free optionality — which is why a
  later-audited venue can call it without re-opening core's audit.
- **`skim_fee` is core-internal** for `execute_write`. For venues, one tiny
  permissionless instruction: `deposit_protocol_fee(mint, amount)` →
  treasury. Anyone being able to *pay* the treasury is harmless.
- **Core is independently launchable**: the quote-based `execute_write` MM
  flow (the current retail product, calls + puts, writer and trader flows) needs
  nothing else. First audit + first release.

Instructions: `initialize`, `set_fee_bps`, `set_admin`, `withdraw_treasury`;
`create_account`, `deposit`, `withdraw`, `rotate_signing_key`, `prune_nonce`;
call bucket: `create_bucket`, `execute_write` (FlowKind arg), 
`write_collateralized`, `exercise`, `redeem_position`, `burn_expired_option`,
`invalidate_bucket`, `revalidate_bucket`, `cleanup_bucket`,
`transfer_position`; put bucket: the mirrors of the above (shared internals
where the Move code shares them); `deposit_protocol_fee`.

### 5.2 `auction_venue` — audit package 2

The three Move auction modules (`rfq.move`, `rfq_put.move`,
`swap_auction.move`) are one machine with different legs — same escrow,
reserve floor, strict-increment (ceiling-division), anti-snipe, push-refund,
coupled-finalize code, three times. On Solana: **one generic auction
primitive plus thin settle modes**.

**Generic auction state** (no knowledge of options): `escrow_mint`/`escrow`
(what the seller locked), `bid_mint`/`bid_escrow`, reserve, deadline +
anti-snipe params, `min_increment_bps`, `best_bidder`, recipients, `origin`,
and — replacing Sui's `coupled: bool` — `settle_authority: Option<Pubkey>`.
When set (the vault PDA), `finalize` requires that authority as a **CPI
signer**; when unset, settle is permissionless. Exact analog of "coupled
auctions can only resolve through the venue's settle path".

**Settle modes:**

1. **Pure swap** (subsumes `swap_auction.move`): escrow → winner, winning bid
   → seller/authority. Zero core dependency — the standalone mode; the venue
   is a usable generic token-auction product on its own. (The Move swap
   auction was venue-coupled-only; the generic program naturally also
   supports uncoupled swaps — free functionality, flagged as in-scope for the
   auditor.)
2. **Covered-call settle** (adapter): CPI `options_core::write_collateralized`
   with the escrowed underlying; call tokens → winner, Position →
   `position_recipient`, net premium → `proceeds_recipient`, fee skim → core
   treasury via `deposit_protocol_fee`.
3. **Cash-secured-put settle** (adapter): same with collateral/premium both in
   settlement mint; validates `collateral == required_collateral(bucket,
   amount)` at create by reading the bucket account + `options_math`
   (read-only deserialization, no CPI).
4. **Expired-bucket recovery** (adapter): refund both escrows when the
   referenced bucket is expired/invalidated — reads bucket state only.

Auction-creation params that referenced the bucket (`SETTLE_BUFFER_MS` vs
expiry, invalidation check, `MIN_DURATION_MS`) live in the adapters' create
paths; the generic create takes plain deadlines.

### 5.3 `options_vault` — audit package 3

Keeps every accounting rule from `vault.move`: pps at `PPS_SCALE` (1e12),
floor on share mint and withdraw, profitable-round fee gate with
mgmt-first/perf-clamp, withdrawals-then-deposits queues at `pps[round]`,
receipt-round conventions (deposits convert at `pps[round − 1]`), aggregate
share mint at finalize, deferred config updates, `update_oracle_feeds` escape
hatch (Settling-only, pending-config kept in sync), Pyth-banded
`select_bucket` with the selling-window hard cap, `maybe_enter_settling`
liveness for bucketless rounds.

Every in-package interaction becomes a CPI or an account read:

| Move (in-package) | Solana (cross-program) |
|---|---|
| `rfq::create_coupled(...)` | CPI `venue.create` with `settle_authority = vault PDA`, escrow funded from the deployable vault; `open_rfqs += 1` |
| `rfq::finalize` returns `(winner, Balance, receipt)` in-process | `vault.settle_rfq` CPIs `venue.finalize` (vault PDA signs); venue transfers escrows to vault token accounts / winner, CPIs core `write_collateralized` with `position_owner = vault PDA`, returns `(bidder, amounts, position_key, range)` via CPI return data; vault does its own accounting in the same instruction |
| swap-auction coupled finalize + fresh-Pyth band check | Same pattern in pure-swap mode; the fresh-cross band check stays in the **vault** (vault policy, not auction mechanics) |
| `ObjectTable<u64, Position>` FIFO | `["vault_pos", vault, index]` PDAs; `crank_redeem` CPIs `core.redeem_position` signed by the vault PDA |
| `bucket::strike/expiry/invalidated` reads | Read-only deserialization of the core bucket account (owner-checked against core program id) |
| `Table pps` | `["round", vault, round]` PDAs |

The **oracle module lives in the vault program** — it is the only consumer
(core and venue never price anything), keeping core's audit free of oracle
risk entirely.

Instructions: `create_vault`, `update_config`, `update_oracle_feeds`,
`pause_deposits`/`unpause_deposits`, `deposit`, `claim_shares`,
`initiate_withdraw`, `complete_withdraw`, `instant_withdraw_pending`,
`crank_redeem`, `select_bucket`, `open_rfq`, `settle_rfq`,
`settle_rfq_expired`, `open_swap_rfq`, `settle_swap_rfq`, `finalize_round`.

---

## 6. Shared `options_math` crate

`pow10` (cap 38), `apply_strike` (round-half-up, calls),
`apply_strike_ceil`/`apply_strike_floor` (puts — with the solvency proof from
`put_bucket.move` preserved as a comment), fee floor in u128, pps math,
`settlement_notional` (half-up) vs `settlement_to_underlying` (floor),
bid-increment ceiling division.

A no-std, dependency-free crate with golden-vector tests (shared with
`vault-sim`/mm-bot vectors — the repo already has this cross-validation
habit). Compiled into each program: auditors review ~200 lines of pure
functions once; every program inherits them. This is the **only** code shared
across audit packages.

---

## 7. Test port plan

**Framework: Rust integration tests on LiteSVM** (fast in-process SVM,
arbitrary clock warping, arbitrary account injection). `sui::test_scenario`
maps almost mechanically: `next_tx(sender)` → build/send tx signed by that
keypair; `clock::increment_for_testing` → set the Clock sysvar;
`#[expected_failure(abort_code = …)]` → assert the specific Anchor error.

Two structural improvements over the Move tests fall out for free:

1. **No oracle test hooks.** Sui couldn't forge a `PriceInfoObject`, so
   `vault.move` grew six `*_with_spot_for_testing` twins. On LiteSVM you
   write a forged `PriceUpdateV2` account with any price/conf/publish-time —
   the **real** oracle-gated entrypoints get tested, including feed-pinning
   and staleness rejections.
2. **No `verify_skip_sig`.** Tests sign real quotes with `ed25519-dalek` (the
   helper pubkeys in `test_helpers.move` are the RFC 8032 test vectors —
   reuse their secret keys) and build real precompile instructions, so the
   actual verification path is exercised, k1/r1 included.

Port map (142 tests, 1:1 unless noted), re-homed per program:

| Move file | Tests | Target program | Notes |
|---|---|---|---|
| `test_helpers.move` | — | shared `tests/helpers.rs` | named keypairs, `init_protocol`, `new_bucket`/`new_put_bucket` (mint creation replaces `create_treasury_cap_for_testing`), quote-signing + Pyth-forging helpers |
| `bucket_tests` | 42 | core | cursor/rounding/strike-scale/invalidation edges — the heart of the suite |
| `put_bucket_tests` | 19 | core | solvency/dust/ceil-floor vectors |
| `quote_tests` | 6 | core | rewritten for precompile introspection: valid sig per scheme, wrong pubkey, tampered message, missing precompile ix, **wrong-precompile-order spoofing** (new, Solana-specific) |
| `account_tests` | 10 | core | nonce PDA create/prune replaces dynamic-field checks |
| `admin/treasury/position` | 7 | core | + new `transfer_position` tests |
| `rfq_tests` / `rfq_put_tests` | 18 / 7 | venue | + outbid-refund ATA handling, settle-recipient ATA verification, settle-authority gating, standalone-swap-mode tests |
| swap-auction cases (from `vault_tests`) | — | venue | extracted to the generic-auction suite |
| `vault_tests` | 29 | vault | spot-injection hooks → forged-Pyth tests |
| `oracle_tests` | 12 | vault | `cross_from_prices` vectors port verbatim |
| `e2e_tests` | 2 | integration | promoted to full three-program LiteSVM tests |

Solana-only additions: (a) golden-vector unit tests locking `options_math`
against the `vault-sim`/mm-bot vectors; (b) rent/close lifecycle tests
(position close, receipt close, nonce prune, auction close) — new surface Sui
didn't have; (c) a **CPI-consumer harness test** in Phase 1: a throwaway test
program calling `write_collateralized` via CPI, proving the composability
surface before the venue exists; (d) pps cross-check against the `vault-sim`
ledger on shared scenarios.

---

## 8. Phases = audit/release packages

Each phase compiles, passes its ported tests, and is independently
reviewable.

1. **Phase 0 — workspace + interface freeze** (small): three-program Anchor
   workspace, `options_math` + golden vectors, and a written **interface
   freeze** for core's CPI surface (`write_collateralized` call/put,
   `redeem_position`, `deposit_protocol_fee`, account layouts, CPI return
   data shapes). Freezing this before Phase 1 is what lets core be audited
   without knowing venue internals.
   → verify: math vectors green; interface doc reviewed.
2. **Phase 1 — `options_core`** (large): everything in §5.1 + ported tests
   (~84 tests) + the CPI-consumer harness.
   → **Audit package 1. Releasable alone: quote-based MM writes for calls +
   puts, exercise/redeem.**
3. **Phase 2 — `auction_venue`** (medium): generic auction + three settle
   modes + adapters; ported RFQ/put-RFQ/swap tests + Solana-specific cases.
   → **Audit package 2** (scope: generic machinery standalone + adapters
   against the already-audited core). **Releasable: permissionless RFQ
   venue, usable with or without the options protocol.**
4. **Phase 3 — `options_vault`** (large): vault + oracle; ported
   `vault_tests` / `oracle_tests` / `e2e_tests` as three-program integration
   tests; pps cross-check vs `vault-sim`.
   → **Audit package 3. Releasable: vault product.**
5. **Phase 4 — hardening** (medium): cross-program threat pass on the new
   attack surface the split creates — fake-program substitution (Anchor
   `Program<>` / owner checks on every cross-program account), CPI
   return-data spoofing, settle-authority bypass, vault counter drift if a
   coupled auction is settled out-of-band, mint-check on every burn path,
   precompile introspection, ATA verification, close-authority on every
   closed account; optional property/fuzz tests on cursor + rounding
   invariants (e.g. put-bucket solvency under random
   write/exercise/redeem sequences); devnet deploy scripts.

Rough proportions: Phase 1 ≈ a third of the work; Phase 3 ≈ a quarter; tests
are ~half the total effort throughout (matching the Move ratio: 5.3k test
lines vs 4.6k source).

---

## 9. Open decision points

1. **Signing schemes**: keep all three (Ed25519/k1/r1) or trim? Recommend
   Ed25519 + secp256k1; drop secp256r1 unless something signs with it today
   (r1 precompile availability on the target cluster is the constraint).
2. **Position representation**: plain PDA record with `owner` +
   `transfer_position` (recommended) vs Token-2022 NFT. The record loses
   NFT-marketplace composability; wrap later if ever needed.
3. **Auction refunds**: push-to-ATA (recommended, parity with Sui) vs
   pull-refund escrow. Documented freeze-authority grief edge.
4. **Bucket PDA salt**: strict one-bucket-per-(pair, expiry, strike, scale)
   via pure PDA seeds vs a salt for re-creation flexibility (Sui allowed
   duplicates; recommend the salt).
5. **Venue fee policy in standalone mode**: option-settle adapters skim
   core's `fee_bps` into core's treasury (parity). Pure-swap standalone
   auctions: recommend **no fee for v1** (matches Move, where swap auctions
   were never fee'd) — avoids giving the venue its own admin surface.
6. **Adapter placement**: inside venue (recommended) vs a fourth
   micro-program (only if the venue binary must be literally core-free).
7. **Interface freeze discipline**: once core is audited, its CPI surface is
   append-only. Anything venue/vault might need goes into the Phase 0
   interface doc — retrofitting core post-audit defeats the staging strategy.

---

## 10. Off-chain impact (out of scope for the port, plan-relevant)

- **options-scheduler**: drops the entire per-roll coin-package
  codegen/publish/cap-harvest pipeline; `create_bucket` creates the mint.
- **quoting service / mm-bot**: re-sign Borsh canonical bytes instead of BCS;
  precompile instruction added to execution transactions.
- **indexer**: consumes Anchor CPI events instead of Sui events; same event
  vocabulary (49 events).
- **keeper**: same crank vocabulary; transactions now prepend Pyth receiver
  price updates instead of Sui Pyth pushes.
