# Collateral Abstraction — Plan

Move ALL quote-driven collateral management onto a dependency-inverted
hot-potato system: core verifies quotes and demands collateral; *what*
holds the collateral is any external package that depends on core — core
never depends on it. Ships with one first-party implementation (the
"simple" MM collateral account) that reproduces today's behavior exactly,
deployed **per market maker** rather than instantiated inside our
protocol.

Companion to `01`–`03`. Assumes the four-package layout is live.

## 1. Why

Today `options_core::account::Account` does two unrelated jobs: quote
authorization (signing pubkey + nonce table) and collateral custody
(balances as dynamic fields that `execute_write` debits). Custody is the
part third parties may want to reinvent — margin vaults, LP-backed
accounts, lazy unwinding — and the part that bloats core's audit
surface. Authorization is the part that must never leave core.

Move has no dynamic dispatch, so the inversion uses the flash-loan
receipt pattern: core mints a no-abilities `CollateralRequest` potato
after verifying a quote; an external package releases funds against a
reference to it; core consumes potato + funds in the same transaction.
Atomicity is structural — the potato cannot be dropped or stored, so
either the write completes with collateral delivered or everything
(including the nonce consumption) reverts.

## 2. Core changes (`options_core`)

### 2.1 `Account` → `QuoteSigner` (custody removed)

Keep: `owner`, `signing_scheme`, `signing_pubkey`, the consumed-nonce
dynamic fields, `create_and_share`, `set_quote_signing_key`,
`prune_nonce`, and the `SignerCreated` / `SigningKeyRotated` events.

Delete: `BalanceKey` balances, `deposit` / `withdraw` /
`withdraw_internal` / `deposit_balance` / `balance_of`, and the
`AccountDeposit` / `AccountWithdraw` events. Core holds **no MM funds**
after this change (buckets and the treasury still custody protocol
funds).

### 2.2 `Quote` gains the collateral source

```move
public struct Quote has copy, drop {
    protocol_id: vector<u8>,
    signer_id: ID,               // the QuoteSigner (was signer_account_id)
    collateral_source: ID,       // NEW: the object release() debits
    release_package: address,    // NEW: package containing release()
    release_module: String,      // NEW: module containing release()
    signer_token_recipient: address,
    bucket_id: ID,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
}
```

The full release routing — source object AND target package/module — is
**inside the signed payload**. Executors build the release call straight
from quote fields; nothing routes on unsigned hints, so no intermediary
(including our own quoting service) can corrupt an MM's routing to
sabotage their fills. This changes the BCS signing payload — every
off-chain signer follows (§5). One signer key may serve many collateral
sources (e.g. per-asset accounts); the MM binds them per quote.

### 2.3 The potato

```move
// options_core::collateral (new module)
public struct CollateralRequest<phantom T> {     // NO abilities
    quote: Quote,
    amount: u64,
    source: ID,
}
public fun amount<T>(r: &CollateralRequest<T>): u64
public fun source<T>(r: &CollateralRequest<T>): ID
public fun quote_nonce<T>(r: &CollateralRequest<T>): u64   // MM-side bookkeeping
```

Request-minting lives with the bucket modules because the demanded
amount is flow- and product-specific (put collateral needs the bucket's
strike math):

| Flow | Function | Demands |
|---|---|---|
| call, writer flow (signer = Trader MM) | `bucket::request_writer_flow<U,S,C>(bucket, signer, config, sq, clock)` | `CollateralRequest<S>` of `quote.premium` |
| call, trader flow (signer = Writer MM) | `bucket::request_trader_flow<U,S,C>(…)` | `CollateralRequest<U>` of `quote.write_amount` |
| put, writer flow | `put_bucket::request_writer_flow<U,S,P>(…)` | `CollateralRequest<S>` of `quote.premium` |
| put, trader flow | `put_bucket::request_trader_flow<U,S,P>(…)` | `CollateralRequest<S>` of `required_collateral(bucket, write_amount)` |

Each verifies signature/expiry/protocol-id, **consumes the nonce**,
checks `quote.bucket_id == id(bucket)` + bucket alive, and mints the
potato with `source = quote.collateral_source`.

### 2.4 `execute_write` consumes the potato

The account-debit variants are deleted; the flow discriminator is now
the request's type + which side the executor coin is on:

```move
public fun execute_write<U, S, C>(          // writer flow
    bucket: &mut Bucket<U,S,C>, config: &ProtocolConfig, treasury: &mut Treasury,
    request: CollateralRequest<S>,          // MM premium, from release()
    premium_funds: Balance<S>,
    underlying_in: Coin<U>,                 // executor side
    position_recipient: address, call_token_recipient: address,
    clock: &Clock, ctx: &mut TxContext,
)
// + trader-flow twin (request: CollateralRequest<U>, funds: Balance<U>,
//   premium_in: Coin<S>) and both put twins.
```

Checks: `funds.value() == request.amount`, `request.quote.bucket_id ==
id(bucket)` (again — the potato could theoretically be routed at a
different bucket object of the same type), then the existing fee-skim /
cursor / mint / event logic unchanged. `WriteExecuted` /
`PutWriteExecuted` events: `signer_account_id` → `signer_id`, plus a new
`collateral_source: ID` field.

### 2.5 Unchanged

The RFQ adapters, generic auction, and vault paths already deliver
collateral as explicit `Balance` via the public collateralized-write
surface — no changes. Exercise/redeem/burn unchanged.

## 3. The standardized release interface

Convention (enforced socially + by the gas-station template shape, not
by core):

```move
/// REQUIRED signature shape, in any module of any package:
public fun release<T>(
    account: &mut <ImplementationType>,      // implementation-specific
    request: &CollateralRequest<T>,          // core's potato, by reference
    ctx: &mut TxContext,
): Balance<T>
```

Rules an implementation must follow:
- Function name is exactly `release` with exactly one type argument
  (this is what the gas-station wildcard pins, §6).
- MUST `assert!(core::collateral::source(request) == object::id(account))`
  — release only what a core-verified quote naming *this* object
  demands. The request reference is the proof: only core mints them,
  post-verification, and the potato guarantees single use per nonce.
- Returns exactly `amount(request)` (core re-checks; returning less
  aborts the whole tx).

Security: a malicious implementation can only refuse (abort) — the same
griefing power an MM has today by withdrawing before execution. The
counterparty is protected by core's amount/type checks, not by trusting
the implementation. No registry, no admin approval on-chain.

## 4. The "simple" MM collateral account (`contracts/mm-collateral/`)

A first-party implementation with **no abilities beyond current
behavior** — it is the old Account custody code relocated:

```move
module mm_collateral::mm_collateral;

public struct CollateralAccount has key {
    id: UID,
    owner: address,          // = publisher, set in init
    // Balance<T> dynamic fields keyed by BalanceKey<T>, as before
}
```

- **One MM per deployment.** `init` creates and shares a single
  `CollateralAccount` owned by the publisher. No public constructor —
  a new MM publishes their own copy of the package. The package id is
  the MM's identity boundary; nothing in our protocol enumerates them.
- Functions: `deposit<T>(account, Coin<T>)` (permissionless),
  `withdraw<T>(account, amount, ctx): Coin<T>` (owner-only),
  `balance_of<T>`, and the standardized `release<T>` (§3 rules).
- Events: `Deposited` / `Withdrawn` / `Released` — for the MM's own
  tooling. NOTE: these carry per-MM package ids, so **our indexer does
  not decode them** (§7).
- Lives in the repo as a template package (`Move.toml` dep:
  `options_core = { local = "../core" }`), added to the Move CI matrix
  with its own tests (release against valid/foreign request, owner
  gating, exact-amount split). It is NOT published by
  deployment-manager — MM tooling publishes it per deployment (§8).

## 5. Quote wire format + PTB composition

The WS quote JSON mirrors the new signed fields (decimal strings /
hex as per the existing conventions):

```json
"quote": {
  …,
  "collateral_source": "0x<CollateralAccount object id>",
  "release_package": "0x<pkg>",
  "release_module": "mm_collateral"
}
```

No unsigned routing envelope: the PTB builder targets
`{release_package}::{release_module}::release<T>` directly from the
verified quote, resolving only the source object's shared version via
RPC. A quote whose routing is wrong (package doesn't exist, module has
no conforming `release`, source mismatch) simply fails to execute — it
can never move the wrong funds, and the MM signed the routing, so the
failure is attributably theirs.

The executor PTB for a wallet trade becomes:

```
quote::new_quote → quote::new_signed_quote
→ bucket::request_writer_flow            (mints the potato)
→ <target>::release                       (MM-specified, wildcarded)
→ bucket::execute_write                   (consumes potato + funds)
```

Builders that follow: `frontend/src/tx/composer*.ts`,
`crates/sui-tx/src/tx/execute_write*.rs`, and the quoting-service's
quote revalidation (BCS layout).

## 6. Gas station: wildcard release template

Extend the template matcher with a target kind that pins **function
name + type-arity only**:

```rust
enum TargetMatcher {
    Exact(MoveTarget),
    /// Any package, any module — function name and type-arg count pinned.
    AnyRelease { function: &'static str /* "release" */, type_args: usize /* 1 */ },
}
```

The `write`/`buy`/`put_write`/`put_buy` templates become: pinned quote +
request + flow-marker + `execute_write` targets (all on known packages),
plus exactly ONE `AnyRelease` slot in `required`/`allowed`. Everything
before and after the release call stays a closed set, so a foreign call
cannot ride along anywhere else in the PTB.

Sponsor risk analysis (document in template.rs like the existing
posture notes): the wildcard call receives only the potato reference and
the MM's own object — it cannot touch sponsor or executor assets not
passed to it. The sponsor risks gas alone, as today; a pathological
`release` that burns compute is bounded by the existing
`max_gas_budget_mist` cap. Template tests: matching happy path, a PTB
with two wildcard calls rejected, a wildcard with the wrong function
name / type-arity rejected.

## 7. Quoting service: balance tracking removed entirely

Today the quoting service tracks MM `available_balance` from core
account events and nets in-memory reservations against it, rejecting
quotes that would oversubscribe an MM. Under the abstraction this is
conceptually unsound, not just operationally awkward: a collateral
implementation need not have a readable balance at all (an LP-backed
account may source liquidity at release time). Any tracking would only
ever cover the simple account while implying coverage of everything.

Decision: **remove balance tracking AND the reservation subsystem.**

- The quoting service becomes pure routing + reputation — its stated
  design identity. Quote validation shrinks to: signature vs the
  registered signing key, expiry, nonce-unseen, bucket sanity, and the
  new routing fields present.
- Enforcement is where it always really was: the on-chain revert. The
  revert-rate reputation system becomes the quality filter with teeth
  (filter/deprioritize MMs whose fills revert); MMs self-manage
  inventory since every revert costs them fill rate and standing.
- WS protocol: `AccountStateUpdate`, `ReservationConfirmed`,
  `ReservationReleased` messages and the reservation TTL eviction task
  are deleted. The spec's oversubscription scenario (§9.2 Scenario 1)
  mitigation becomes reputation-only — update the spec text.

Indexer: drop the `accounts` balance materialization + account
deposit/withdraw decoding; keep `SignerCreated`/`SigningKeyRotated`
(renamed from AccountCreated). `WriteExecuted`'s new
`collateral_source` field flows into the events payload for
attribution and revert/fill accounting.

## 8. mm-bot: deploy, don't create

Replace "create an Account object in our protocol" with "publish your
own collateral package":

- New subcommand `mm-bot deploy-collateral --network … --secrets …`:
  compiles `contracts/mm-collateral/` against the env's published
  `options_core` (same `BuildConfig` + Published.toml machinery as
  deployment-manager — extract the small publish helper into a shared
  crate rather than duplicating), publishes it, and records
  `{package_id, account_id}` into the bot's config/state. The Docker
  image ships the Move sources for this.
- Startup: still `create_and_share` a `QuoteSigner` in core (or reuse);
  config now carries `collateral_package` + `collateral_account`.
- Quote signing: include `collateral_source` + `release_package` +
  `release_module` in the BCS payload (and the mirrored WS quote
  fields).
- Funding: `deposit` into its own account (replaces core
  `account::deposit`); the balance-monitor's account checks follow.
  With `AccountStateUpdate` gone (§7), the bot tracks its own available
  funds locally — it is the owner; RPC-reading its own account is
  its own concern, not protocol infrastructure.

## 9. Sequencing

```
PR 1  contracts: core rework (QuoteSigner, Quote.collateral_source,
      CollateralRequest, request_* fns, execute_write twins) +
      contracts/mm-collateral template package + tests + Move CI
PR 2  off-chain: sui-tx builders + gas-station wildcard matcher +
      quoting-service (wire format, RPC balance polling) + mm-bot
      (signing, deploy-collateral, funding) + indexer event changes
PR 3  frontend: composer PTBs (request → release → execute_write),
      quote envelope handling
PR 4  docs: spec v0.3 (Quote layout, potato protocol, release
      convention) + 03-package-specs addendum
```

Same redeploy posture as the four-package split: nothing breaks staging
at merge; the redeploy is the atomic point (fresh Quote BCS layout means
old signed quotes are invalid by construction — protocol_id/domain
separation already covers cross-deployment replay).

## 10. Risks / accepted behaviors

- **Composer awareness is inherent** (no dynamic dispatch): whoever
  builds the PTB must name the release target. Contained by the quote
  envelope carrying the target and the standardized signature; retail
  users never see it.
- **Gas-station sponsors calls into unreviewed packages.** Gas-only
  risk (see §6); acceptable, same posture as sponsoring user-owned
  asset moves today.
- **No pre-trade feasibility check** (§7): retail may occasionally be
  routed a quote that reverts on execution. This was always possible
  (the old check was optimistic); the compensations are reputation
  filtering with revert-rate teeth and MM self-interest. Watch the
  realized revert rate on staging before prod.
- **Per-MM deployment friction** is deliberate: the package id is the
  MM's trust boundary and there is no shared custody object to attack
  or to audit for cross-MM isolation. The `deploy-collateral`
  subcommand keeps onboarding to one command.
- **Auditor framing**: core's potato protocol is the new critical
  surface (single-use per nonce, amount/type binding, bucket binding);
  the simple account is a ~150-line relocation of already-audited
  custody code.
