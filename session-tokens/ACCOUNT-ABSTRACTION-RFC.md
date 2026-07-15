# RFC: Session Tokens as a Full-Custody Account-Abstraction Layer

**Status:** Draft / design-only. No code changes proposed for this round — this
documents *what would change* to let third-party dApp builders integrate the
session-key system as a standalone account-abstraction (AA) layer.

**Scope decision (this RFC):** full-custody model. User funds live in the
per-user session `Account`; integrating dApps move funds out of custody under a
`SessionCap`'s per-type limits. (The alternative "delegated-authz-only" model,
where the dApp keeps custody and the session layer is a pure authorization
oracle, is noted in §7 as the natural future tier but is **not** the target
here.)

---

## 1. Premise check: the Account is per-user (confirmed)

`Account` is keyed by `owner_pk` (32-byte Solana ed25519 pubkey or 20-byte
Ethereum address) in `registry.accounts`. Sign-in is find-or-create
(`session::open_for`, session.move:189): the first sign-in for an identity mints
and shares the `Account` and registers it; every later sign-in for that same
identity **reuses** it. `generation` revokes every outstanding cap on that
account at once.

```
one root identity (Solana/Eth pubkey)
        │   registry.accounts[owner_pk]
        ▼
one shared Account  ── holds Balance<T> per coin type (dynamic fields)
        ▲                spent ledger keyed by (cap_id, coin_type)
        │
many SessionCaps (one per ephemeral key / device / dApp session)
        └─ all killed together by a generation bump
```

So full custody is coherent: a user funds **one** account and any number of
sessions — across any number of integrating dApps — spend from it under their
own caps and limits.

---

## 2. Today: what's already a platform, and what isn't

### Already decoupled (reusable as-is)
- The session package (`registry`, `account`, `session`, `message`, `siwe`,
  `errors`) has **zero** options-protocol dependency.
- The authorization seam is already public and generic:
  - `session::authorize(cap, account, clock, selector, sender)` — non-spending.
  - `session::authorize_spend<T>(cap, account, clock, amount, selector, sender)`
    — checks + records per-(cap, type) spend against the cap's signed limits.
- Funding is already permissionless and multi-asset: `account::deposit<T>` and
  `account::receive<T>` (sweep stray `AddressOwner` coins) are public.
- The SDK is config-driven (`SessionConfig` carries packageId / registryId /
  network) and the only SuiOptions-specific piece — `suiOptionsGasStation` — is
  isolated in `sponsor.ts` behind the `GasStationAdapter` interface.

### The blocking gap for third-party full custody
`account::take<T>` is `public(package)` (account.move:133) and
`authorize_spend<T>` returns nothing. **A third-party package cannot remove
funds from the Account.** The only code that can is an entrypoint compiled into
the session package itself — which is precisely why `app_example::withdraw`
lives inside it, and why options had to embed `_with_session` twins that the
session package is aware of.

> **Consequence:** as written, "integrating" means *adding code to the session
> package* (or forking it). That is not a platform other builders can adopt.

### Secondary couplings
1. **Per-app branding is hardcoded.** `siwe.move` bakes `siws-session.demo` as
   the domain/URI/statement the wallet shows at sign-in. Every dApp's user would
   see the same string. The SIWS path is domain-bound to the registry address,
   which is fine, but the human-readable wallet prompt is not per-dApp.
2. **ABI stability is implicit.** Third-party Move packages would depend on a
   published session package; `authorize` / `authorize_spend` / the new spend
   primitive (§3) must be treated as a frozen ABI across upgrades. The Registry
   already survives upgrades; the package id does not (builders pin a version).
3. **Sponsor PTB allowlist is per-deployment.** Each new integration's PTB shape
   must be payable by *some* sponsor; SuiOptions' gas station requires shapes to
   be pre-registered in sui-tx `template.rs`.

---

## 3. The core change: a public, cap-gated custody-spend primitive

The one Move change that turns this from "an options feature" into "an AA layer"
is exposing fund extraction as a **public** function that any package can call,
still fully gated by the cap. Two viable shapes:

### Option A — `spend<T>` returns the coin (simple)
```move
/// authorize_spend + take, exposed publicly. Returns the Coin so the calling
/// package can settle it however it wants (transfer, deposit into its own
/// protocol, split across a PTB, …). Fully cap-gated; the sender binding and
/// per-type limit checks are identical to authorize_spend.
public fun spend<T>(
    cap: &SessionCap,
    account: &mut Account,
    clock: &Clock,
    amount: u64,
    selector: vector<u8>,
    sender: address,
    ctx: &mut TxContext,
): Coin<T> {
    authorize_spend<T>(cap, account, clock, amount, selector, sender);
    account::take<T>(account, amount, ctx)   // requires `take` stays in-package
}
```
- `account::take<T>` stays `public(package)` — only `session::spend` reaches it.
  No new way to drain custody outside a cap check.
- The third-party package receives a `Coin<T>` and does anything with it.
- **Risk to call out:** once the coin is in the caller's hands, the limit has
  already been *recorded*, so a caller that aborts after `spend` would have
  consumed budget for a transfer that rolled back — but the whole PTB aborts
  atomically, so the recorded spend rolls back too. Safe.

### Option B — hot-potato settlement receipt (stricter)
Return a non-`drop` receipt that must be consumed by a matching
`settle`/`deposit`, forcing the caller to account for the coin within the same
PTB. More ceremony; only worth it if you want to constrain *where* spent funds
may go. **Recommendation: ship Option A**; the cap's `allowed` selector list
already bounds *which* functions can spend, which is the meaningful control.

### Settling back into custody
Already covered: outputs return to custody via the public `account::deposit<T>`
(this is exactly how options settles trade outputs back into the Account). No
change needed.

### Net Move surface a third-party integrator uses
```
account::deposit<T>            // fund custody            (exists, public)
account::receive<T>            // recover stray coins      (exists, public)
account::balance_of<T>         // read                     (exists, public)
account::spent_of<T>           // read                     (exists, public)
session::authorize             // gate a no-spend action   (exists, public)
session::authorize_spend<T>    // meter a spend            (exists, public)
session::spend<T>  ◄── NEW     // meter + extract a Coin   (the gap)
session::limit_of<T>           // read a cap's limit       (exists, public)
```
A dApp's integration becomes: *depend on the published session package, write
your own entrypoints that take `(&SessionCap, &mut Account, &Clock, …)`, declare
your own `pkg::module::function` selector, and call `session::spend<T>` (or
`authorize` for non-spend actions).* No edits to the session package.

---

## 4. De-branding the wallet prompt (per-dApp identity)

For a multi-tenant layer, the SIWS/SIWE message builders need a per-dApp
`domain` / `statement` / `uri` so the wallet shows "**acme.fi** wants you to
sign in," not "siws-session.demo." This means:
- Thread an app-supplied display domain through `message::build_session_message`
  / `siwe::build_message` (and the revoke variants), **covered by the
  signature** so it can't be spoofed.
- Keep the security-critical `domain` (= registry address, the cross-contract
  replay guard) separate from the cosmetic display domain. Don't conflate them.
- The TS serializers (`message.ts`, `siwe.ts`) move byte-for-byte in lockstep;
  regenerate the shared reference vectors (`gen-siwe.mjs`) and re-pin both test
  suites. **This is the highest-risk seam** — any divergence silently breaks
  sign-in.

Open question for §6: is the display domain a free-form per-call string, or
registered once per dApp in the Registry (so it can't be set to an arbitrary
brand by a phishing front-end)? Registering it is safer but adds an onboarding
step and Registry write.

---

## 5. Productizing the three layers

### Layer 1 — Move package (the AA primitive)
- Add `session::spend<T>` (§3, Option A).
- Add per-dApp display domain to the signed message (§4).
- Publish standalone with a documented **ABI-stability contract** for the public
  functions. Keep `account::take` package-private.
- Provide a copy-paste integration module (today's `app_example` promoted to a
  documented template) showing a spend entrypoint and a non-spend entrypoint.

### Layer 2 — SDK (`@yourorg/sui-siws-session` → real published package)
- Already config-driven; lift it out of `frontend/siws-session-sdk/` (currently
  "consumed from source" inside the app) into a versioned npm package.
- Keep `GasStationAdapter` as the sponsor extension point; ship
  `suiOptionsGasStation` as a *separate* optional adapter, not a core import.
- `SessionHandle.execute(build)` already lets an integrator inject arbitrary
  `moveCall`s — a third-party dApp builds its own `spend`/app PTB inside the
  callback. No core SDK change needed beyond packaging + the per-dApp domain in
  `createSession` options.

### Layer 3 — Sponsor (the real operational lift)
Full custody doesn't change the sponsor's power (it still only pays gas and
co-signs; it never holds a cap), but a platform needs *someone* to pay gas for
each integration's PTB shapes. Options, lightest-first:
1. **BYO-sponsor (ship first).** Each dApp runs its own relayer implementing
   `GasStationAdapter`, with its own target/PTB allowlist. Zero shared infra;
   the SDK already supports it. This is the pragmatic v1 for "other builders can
   integrate."
2. **Hosted multi-tenant sponsor (later).** A relayer with self-serve per-dApp
   registration of allowed targets / PTB shapes and per-dApp gas accounting.
   This is where most of the *new* engineering and ops cost lives; defer until
   there's demand.

---

## 6. Integration walkthrough (what a new dApp builder does, target state)

```
1. Move:
   - add the published session package to Move.toml
   - write entrypoints over (&SessionCap, &mut Account, &Clock, …):
       const SEL: vector<u8> = b"acme::vault::deposit_and_mint";
       public entry fun deposit_and_mint<T>(cap, account, clock, amount, …, ctx) {
           let coin = session::spend<T>(cap, account, clock, amount, SEL, ctx.sender(), ctx);
           // …acme's own logic; settle outputs back via account::deposit<T>…
       }
   - publish acme's package

2. Frontend (SDK):
   - createSession({ …, displayDomain: "acme.fi",
                     limits: [{ coinType, perTx, total }],
                     allowed: ["acme::vault::deposit_and_mint"] })
   - session.execute((tx, { capId, accountId }) =>
       tx.moveCall({ target: "acme::vault::deposit_and_mint", … }))

3. Sponsor:
   - run a GasStationAdapter whose allowlist includes the session entrypoints
     (verify_and_open_session[_eth], revoke_all[_eth]) AND acme's targets.
```

The user signs **once** with Phantom/MetaMask, funds the shared Account once,
and from then on every Acme action is auto-signed by the ephemeral key and
gas-paid by the sponsor, bounded by the cap's per-type limits and selector
allowlist. The same Account/identity works across every integrating dApp.

---

## 7. Positioning & residual decisions

- **Differentiator.** For Sui-native users, zkLogin/passkey + sponsored tx
  already gives "sign once, no per-tx prompts." This layer is genuinely unique
  for the **cross-chain** case: a Solana/Ethereum identity driving a Sui dApp
  without ever holding SUI. Pitch it as *cross-chain onboarding/custody*, not a
  generic AA layer competing with native Sui AA.
- **Full custody is a trust ask.** Every integrating dApp's users park funds in
  the shared session `Account`. That's fine within one trusted protocol
  (options today); across third parties it means a user's balance is reachable
  by *any* cap they've signed — scoped by per-type limits and selector
  allowlists, but shared. The natural future tier is **delegated-authz-only**
  (dApp keeps custody; session layer authorizes via `authorize` with no
  `spend`), which removes the shared-pool trust concern. Worth deciding whether
  full custody is the *only* tier or the *first* tier.
- **Display-domain trust (§4):** free-form vs Registry-registered.
- **Sponsor model (§5 L3):** BYO first; hosted later.
- **ABI freeze:** commit `authorize` / `authorize_spend` / `spend` /
  `account::deposit` signatures as stable before any external builder depends on
  them.

---

## 8. Suggested milestones (when this moves past design)

1. Add `session::spend<T>` + tests (the one change that unblocks third-party
   custody spends). Promote `app_example` to a documented integration template.
2. Per-dApp display domain in the signed message; regenerate + re-pin the
   byte-exact serializer vectors both sides.
3. Extract the SDK to a standalone versioned npm package; split the SuiOptions
   gas-station adapter out of core.
4. Write the integration guide (Move dep + ABI contract + sponsor allowlist).
5. (Later) Hosted multi-tenant sponsor with self-serve PTB-shape registration.
```
