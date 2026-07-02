# Relayer dispatch — design deep-dive & fix plan

**Problem:** a generic relayer can submit to the EVM Inbox for any app, but
cannot submit to the Sui Inbox without app-specific knowledge. Why, and how do
we make one relayer relay *all* transactions?

---

## 1. Deep dive: where the spec's dispatch model breaks

### 1.1 What the spec assumes
The spec (§2.5 step 8, §3.2 step 7–8, §3.3 step 7–8) describes delivery as:

> Inbox … **dispatches the payload to message.dst_app via app callback** …
> Locker(Sui).onReceive(payload) mints …

This is a single mental model: **the Inbox synchronously calls the destination
app by address.** That model is *EVM-shaped*. It is correct for EVM and quietly
impossible for Sui.

### 1.2 Why it works on EVM
EVM has **dynamic dispatch** and **address-rooted storage**:

```solidity
IMessageRecipient(dstApp).onReceive(srcChainId, srcApp, payload);
```

The Inbox holds only `dstApp` (an address) and calls it. The app reads *its own*
storage by address — the caller supplies no app state. So the relayer needs to
know **nothing** about the app: it just calls `Inbox.receiveMessage(message,
envelope)` and the Inbox fans out. **The EVM relayer is fully generic.** (We've
proven this end-to-end on anvil.)

### 1.3 Why it cannot work on Sui
Move/Sui has **no dynamic dispatch** and an **explicit-object argument model**:

- A package can only call functions it imported at compile time. There is no
  `call(address, function, args)`. The Inbox literally cannot name an arbitrary
  `dst_app`'s code.
- Functions operate on **objects passed in as arguments**. To mint, the Locker
  needs its own shared object (and `TreasuryCap`, `Clock`, …) passed *as call
  arguments*. Storage is not address-rooted; it's object-rooted and must be
  supplied by the transaction.

The consequence is that **control flow must invert**. On EVM the Inbox calls the
app; on Sui the **app must be the entry point and call *into* the Inbox** to
verify, then act on its own objects:

```
// EVM:  relayer → Inbox.receiveMessage → (Inbox calls) app.onReceive
// Sui:  relayer → app.bridge_receive → (app calls) inbox::receive + inbox::consume → app effects
```

Our current `Inbox.receive → DeliveredMessage (hot potato) → consume(&UID)` API
is the *correct* Move shape for this. The hot potato forces the app to discharge
the message (replay-safe, atomic), and `&UID` proves the caller is the real
`dst_app`. Nothing is wrong with the contracts.

**The real gap is off-chain:** to build the Sui delivery transaction, the relayer
must name the app's function and pass the app's objects. That information is
app-specific, so a *naively* generic relayer can't construct it.

### 1.4 This is intrinsic, not a bug
This is how every Move bridge works. Wormhole on Sui returns a verified-VAA hot
potato; the Token Bridge app is the entry that consumes it, and Wormhole's
relayers are **app-aware on Sui** (there is no generic on-chain relayer for Sui
delivery — only on EVM). So your instinct ("each app needs app-specific relay")
is correct in direction. The goal is to **shrink that per-app surface to almost
nothing** so one relayer process still serves every app.

---

## 2. Design target

One relayer **process/codebase** that relays everything:

- **EVM destinations:** fully generic (already true).
- **Sui destinations:** generic for apps that follow a **standard receive
  convention** — zero per-app code. A small escape hatch for non-standard apps.

The trust model is untouched: the relayer stays untrusted, every message
self-verifies on-chain, and a wrong dispatch just produces a reverting tx.

---

## 3. The fix

### 3.1 On-chain: a standard Sui "bridge-receive" convention
Define one canonical entry shape that all standard Locker-style apps implement:

```move
// In the app module that also defines the app object's type.
public fun bridge_receive(
    inbox:    &mut Inbox,
    keys:     &GroupKeyRegistry,
    self:     &mut Locker,          // the dst_app object
    message:  vector<u8>,           // BCS of CrossChainMessage
    envelope: vector<u8>,           // BCS of SignatureEnvelope
    clock:    &Clock,
    ctx:      &mut TxContext,
) {
    let m = message::from_bcs(message);
    let env = envelope::from_bcs(envelope);
    let delivered = inbox::receive(inbox, keys, m, env);
    let (src_chain, src_app, payload) = inbox::consume(inbox, delivered, &self.id);
    // app checks src_app == registered peer, decodes payload, mints, rate-limits.
}
```

Key properties:
- **Fixed positional signature** → the relayer builds args without per-app code.
- The app does `receive` + `consume` + effects internally → **one MoveCall** per
  delivery.
- `message`/`envelope` passed as BCS bytes so the relayer supplies plain `vector<u8>`
  args (no need to construct Move structs in the PTB). Needs small
  `message::from_bcs` / `envelope::from_bcs` helpers (L1 addition).
- **No L1 contract change** beyond those two helper constructors — the existing
  `receive`/`consume` already support this.

### 3.2 On-chain: convention-over-configuration discovery (the elegant part)
The relayer does **not** need a per-app config to find the call target. From the
message it has `dst_app` (an object id). It can:

1. `getObject(dst_app)` → the object's type `0xPKG::locker::Locker`.
2. Derive `(package = 0xPKG, module = locker)` from the type.
3. Assume the standard function name `bridge_receive`.
4. Build `0xPKG::locker::bridge_receive(inbox, keys, dst_app, message, envelope, clock)`.

So a **standard app needs zero relayer configuration** — its on-chain type *is*
the dispatch descriptor. New apps "just work" if they follow the convention.

### 3.3 On-chain (optional): descriptor registry for non-standard apps
Apps that need extra objects (an oracle, a second treasury) or a non-standard
module/function register a descriptor in a shared registry:

```
DeliveryRegistry[dst_app] = {
  package, module, function,
  extra_objects: vector<ObjectArg>,   // appended after clock, in order
  mutability: vector<bool>,
}
```

Untrusted: a bad descriptor only yields a failing tx. The relayer reads it when
present, else falls back to the §3.2 convention.

### 3.4 Off-chain: relayer becomes a family-routed dispatcher
Generalize today's single `DestSubmitter` into routing by destination:

```
relay_message(message, signer):
    envelope = signer.sign(message)
    submitter = router.for_chain(message.dst_chain_id)   // by family
    submitter.submit(message, envelope)
```

- `EvmSubmitter` (per EVM chain) — exactly today's `EvmDestSubmitter`, generic.
- `SuiSubmitter` (per Sui chain) — **new**, generic:
  1. Resolve `(package, module, function)` from `dst_app` type (§3.2) or registry (§3.3).
  2. Resolve shared-object args (`Inbox`, `GroupKeyRegistry` from the chain
     registry; `dst_app`; `Clock` = `0x6`; any extras) → fetch
     `initial_shared_version` + mutability via RPC.
  3. BCS-encode `message`/`envelope` to the Move struct layout (add
     `bridge_types::to_move_bcs`).
  4. Build the `MoveCall` PTB, sign with the relayer's Sui key, submit.

Both submitters implement the same `DestSubmitter` trait, so the orchestration,
dedup (`is_delivered` → `inbox::is_consumed`), and retry loop are unchanged.

### 3.5 Off-chain: escape hatch for exotic apps
A `SuiCustomAdapter` trait keyed by `dst_app` (or app type) lets a truly unusual
app ship a small Rust PTB-builder plugged into the same router. This is the only
case that resembles "an app-specific relayer," and it's a ~30-line adapter, not a
new process.

---

## 4. What this means for "app-specific relayers"
Your realization, refined:

| App shape | Relayer work needed |
|---|---|
| Standard Locker (EVM dst) | none — generic |
| Standard Locker (Sui dst), follows `bridge_receive` convention | **none** — type-derived dispatch |
| Sui app needing extra shared objects | one on-chain descriptor row (no relayer code) |
| Sui app with exotic PTB needs | a small `SuiCustomAdapter` (Rust), same process |

So you do **not** run a relayer per app. One relayer relays everything; apps pay
a convention (a standard entry function) instead of a bespoke relayer.

---

## 5. Sequencing
- **No new L1 milestone.** This is M2 (Locker) work.
- **L1 contract changes:** tiny — add `message::from_bcs` / `envelope::from_bcs`
  (Move) and `bridge_types::to_move_bcs` (Rust). The `Inbox` API is unchanged.
- **M2 deliverables gain:** the `bridge_receive` convention in the Locker, the
  generic `SuiSubmitter`, and the family router in the relayer. The EVM submitter
  is already done and stays.
- **Result:** the relayer relays both directions for the Locker (and any
  convention-following app) end-to-end.

## 6. Alternatives considered (and why not)
- **Per-app off-chain config map** (`dst_app → call target`): simpler than a
  registry but needs reconfiguring the relayer for every new app. The
  type-derived convention (§3.2) is strictly better and free.
- **Verify-and-store "mailbox"** (`inbox::verify` stores the message; app pulls
  later): makes the *relayer* generic but it no longer *delivers* — the app must
  run its own keeper to pull, which just relocates the app-awareness and adds a
  second tx + storage. Doesn't meet "relayer delivers everything."
- **On-chain generic dispatch on Sui:** impossible — Move has no dynamic
  dispatch or function pointers across packages. Confirmed dead end.
