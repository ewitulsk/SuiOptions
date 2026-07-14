# Solana Quoting Service — Frontend Integration Guide

The Solana twin of the Sui quoting-service. It brokers RFQs between retail
frontends and market makers over a single WebSocket, validates MM-signed
quotes, and returns them **ready to execute on-chain**: each quote entry
carries the exact bytes and signature the transaction's Ed25519SigVerify
precompile instruction must contain.

- **WS URL:** `wss://<host>/{env}/solana-quoting` (e.g.
  `wss://sui-options.com/staging/solana-quoting`). Local dev:
  `ws://127.0.0.1:9002`.
- **Plain HTTP side doors** on the same port: `GET /health`, `GET /metrics`,
  `GET /rfq-stream` (NDJSON firehose of incoming RFQs, operator tooling).

## Wire conventions

Every frame is JSON: `{ "type": "<Variant>", "request_id"?: "...", "payload": { ... } }`.

- **Ids** (accounts, buckets, mints) are **base58 pubkey strings**, compared
  byte-exact — never normalize case.
- **Integers** (`write_amount`, `premium`, `valid_until_ms`, `nonce`,
  `deadline_ms`, `total_written`, …) are **decimal strings** (JS-safe past
  2^53).
- **Raw bytes** (signatures, auth challenges, signing pubkeys) are
  **`0x`-prefixed hex strings**.
- `quote_bytes_b64` is **base64** (standard alphabet, padded).
- `request_id` is client-generated (any unique string; UUIDs recommended)
  and correlates a request with its response/errors.

The service sends `{"type":"Ping"}` periodically; reply `{"type":"Pong"}`.

---

## Retail message catalog

### Hello (first frame, required)

The first frame on every connection must be a `Hello`. The retail shape
(no `account_id` in the payload — that's what distinguishes it from the MM
Hello) is:

```json
{ "type": "Hello", "payload": { "role": "trader", "version": "1.0.0" } }
```

`role` is `"writer"` or `"trader"`. The service replies:

```json
{ "type": "HelloAck", "payload": { "session_id": "5f0c…-…" } }
```

### SubscribeBuckets → BucketUpdate

```json
{
  "type": "SubscribeBuckets",
  "payload": { "bucket_ids": ["9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin"] }
}
```

One `BucketUpdate` per known bucket:

```json
{
  "type": "BucketUpdate",
  "payload": {
    "bucket_id": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
    "total_written": "5000000000",
    "exercise_cursor": "0",
    "expiry_ms": "1760000000000"
  }
}
```

### RFQRequest → RFQResponse

Ask for executable, signed quotes on one bucket:

```json
{
  "type": "RFQRequest",
  "request_id": "req-42",
  "payload": {
    "bucket_id": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
    "write_amount": "10000000",
    "side": "writer"
  }
}
```

`side` is the **retail** side: `"writer"` = you provide collateral and sell
the option (MMs bid a premium to buy it); `"trader"` = you pay premium to
buy the option (MMs offer to write it). The service broadcasts to connected
MMs, collects for `rfq_window_ms` (2s default), validates + reserves, then
responds:

```json
{
  "type": "RFQResponse",
  "request_id": "req-42",
  "payload": {
    "bucket_id": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
    "write_amount": "10000000",
    "quotes": [
      {
        "quote": {
          "protocol_id": "F7yh…Config-PDA…",
          "signer_account": "3Nf1…MmAccount…",
          "signer_token_recipient": "8kPq…wallet…",
          "bucket": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
          "write_amount": "10000000",
          "premium": "50000000",
          "valid_until_ms": "1748534400000",
          "nonce": "42"
        },
        "signature": "0x8a4e…64 bytes of ed25519 signature…",
        "quote_bytes_b64": "ERER…base64 of the 160-byte canonical Borsh quote…",
        "mm_id": "3Nf1…MmAccount…",
        "mm_reputation": 0.97
      }
    ]
  }
}
```

`quotes` is already sorted best-first for your side (highest premium first
for writers, lowest first for traders; reputation breaks ties). Each quote
is reserved against the MM's balance until `valid_until_ms` — execute
before then.

### BulkViewRFQRequest → BulkViewRFQResponse

Indicative (unsigned, non-executable) premiums for many buckets at once —
drives strike-tile displays. Cached ~30s server-side
(stale-while-revalidate; `stale: true` means a refresh is already underway).

```json
{
  "type": "BulkViewRFQRequest",
  "request_id": "bv-7",
  "payload": {
    "bucket_ids": ["9xQe…", "4rTm…"],
    "write_amount": "10000000",
    "side": "writer"
  }
}
```

```json
{
  "type": "BulkViewRFQResponse",
  "request_id": "bv-7",
  "payload": {
    "write_amount": "10000000",
    "premiums": [
      {
        "bucket_id": "9xQe…",
        "premium": "48000000",
        "mm_count": 2,
        "stale": false,
        "cache_age_ms": "1200"
      }
    ]
  }
}
```

Buckets no MM priced are omitted — keep the tile placeholder.

### Error

```json
{
  "type": "Error",
  "request_id": "req-42",
  "payload": { "code": "no_quotes", "message": "no MMs returned a valid quote within the window" }
}
```

Retail-facing codes:

| code | meaning |
|---|---|
| `unknown_bucket` | Bucket id isn't known to the indexer. |
| `bucket_invalidated` | Admin invalidated the bucket; executing would revert. |
| `indexer_unavailable` | Service couldn't reach solana-indexer; retry. |
| `rate_limited` | Too many in-flight RFQs (per-session or global cap). |
| `no_quotes` | Window elapsed with zero valid quotes. |

---

## MM message catalog (for completeness)

MM clients (the solana-mm-bot) send a `Hello` whose payload **contains
`account_id`**, then complete an ed25519 challenge-response:

```json
{
  "type": "Hello",
  "payload": {
    "roles": ["trader_mm", "writer_mm"],
    "account_id": "3Nf1…MmAccount…",
    "signing_scheme": 0,
    "signing_pubkey": "0xab…32 bytes…",
    "bulk_view": true
  }
}
```

`signing_scheme` is the on-chain u8 tag; **0 (Ed25519) is the only scheme
program v1 supports** — anything else fails auth with
`auth_scheme_unknown`. Flow:

1. Service → `AuthChallenge { "challenge": "0x…32 bytes…" }`
2. MM signs the raw challenge bytes with its registered signing key and
   replies `AuthResponse { "signature": "0x…64 bytes…" }`
3. Service verifies against the `(scheme, pubkey)` the indexer holds for
   that MmAccount and replies `AuthAck { "session_id": "…" }`, or `Error`
   with `auth_scheme_unknown` / `auth_pubkey_mismatch` /
   `auth_signature_invalid`.

After auth the MM receives `RFQBroadcast` / `BulkViewRFQBroadcast` frames
(bucket **addresses only** — the MM resolves strike/expiry/mints from
solana-api-service itself) and answers `Quote` / `Decline` /
`BulkViewQuote` keyed by `request_id`:

```json
{
  "type": "RFQBroadcast",
  "request_id": "req-42",
  "payload": {
    "bucket_id": "9xQe…",
    "write_amount": "10000000",
    "side": "writer",
    "deadline_ms": "1748534402000"
  }
}
```

```json
{
  "type": "Quote",
  "request_id": "req-42",
  "payload": { "quote": { …same shape as above… }, "signature": "0x…" }
}
```

`AccountStateUpdate`, `ReservationConfirmed`, and `ReservationReleased`
frames exist in the protocol but are not currently emitted.

---

## Executing a quote on-chain

On Solana the program cannot verify ed25519 in-program. The executing
transaction must contain **two instructions**:

1. the native **Ed25519SigVerify precompile instruction** carrying the MM's
   signing pubkey, the 64-byte quote signature, and the canonical quote
   bytes as its message (self-contained — the runtime verifies the
   signature before execution), and
2. the `options_core` **`execute_write`** instruction (or
   `execute_put_write` for a put bucket), which introspects instruction #1
   via the Instructions sysvar at the index you pass as `sig_ix_index`.

Everything you need comes straight from the `RFQResponse` entry:

- **message** = `base64decode(entry.quote_bytes_b64)` — do NOT re-serialize
  the quote yourself; these are the byte-exact canonical Borsh bytes
  (4 × 32-byte pubkeys + 4 × u64 LE = 160 bytes).
- **signature** = hex-decode `entry.signature` (64 bytes).
- **pubkey** = the MM's **signing key**, i.e. the `signing_pubkey`
  registered on the MmAccount — NOT the MmAccount address. Fetch it from
  solana-api-service / the indexer's `account(id: entry.mm_id)` query
  (`signingPubkeyHex`).

### 1. Build the precompile instruction

`@solana/web3.js`'s `Ed25519Program.createInstructionWithPublicKey` produces
exactly the layout the program demands (one signature; the
signature/pubkey/message instruction-index fields all `u16::MAX` = 0xffff,
meaning "self-contained"). **Do not pass `instructionIndex`** — leaving it
undefined yields the required 0xffff sentinels.

```ts
import { Ed25519Program } from "@solana/web3.js";

const message   = Buffer.from(entry.quote_bytes_b64, "base64");        // 160 bytes
const signature = Buffer.from(entry.signature.replace(/^0x/, ""), "hex"); // 64 bytes
const mmSigningPubkey = Buffer.from(signingPubkeyHex.replace(/^0x/, ""), "hex"); // 32 bytes

const sigVerifyIx = Ed25519Program.createInstructionWithPublicKey({
  publicKey: mmSigningPubkey,
  message,
  signature,
  // instructionIndex: leave undefined → u16::MAX (self-contained)
});
```

The on-chain verifier (`options_core::quote::verify_ed25519_quote_ix`)
rejects the transaction unless: the instruction at `sig_ix_index` is the
Ed25519 program, `num_signatures == 1`, all three instruction-index fields
are `u16::MAX`, the pubkey equals the MmAccount's registered
`signing_pubkey`, and the message equals the Borsh quote bytes.

### 2. Build `execute_write` and assemble the transaction

Via the Anchor client (args: `quote`, `flow`, `position_recipient`,
`sig_ix_index`):

```ts
const quote = {
  protocolId:           new PublicKey(entry.quote.protocol_id),
  signerAccount:        new PublicKey(entry.quote.signer_account),
  signerTokenRecipient: new PublicKey(entry.quote.signer_token_recipient),
  bucket:               new PublicKey(entry.quote.bucket),
  writeAmount:   new BN(entry.quote.write_amount),
  premium:       new BN(entry.quote.premium),
  validUntilMs:  new BN(entry.quote.valid_until_ms),
  nonce:         new BN(entry.quote.nonce),
};

// Your RFQ side maps to the flow enum: retail "writer" → { writer: {} },
// retail "trader" → { trader: {} }.
const flow = { writer: {} };

const positionKeypair = Keypair.generate(); // Position is a fresh signer account

const executeIx = await program.methods
  .executeWrite(quote, flow, positionRecipient, 0 /* sig_ix_index */)
  .accounts({
    executor: wallet.publicKey,
    config: configPda,                    // == quote.protocol_id
    treasury: treasuryPda,
    bucket: new PublicKey(entry.quote.bucket),
    settlementMint,
    underlyingVault,                      // ATA(underlying_mint, bucket)
    callMint,
    callDest,                             // writer flow: token acct owned by quote.signer_token_recipient
    mmAccount: new PublicKey(entry.mm_id),
    mmSettlement,                         // ATA(settlement_mint, mm_account)
    mmUnderlying: null,                   // trader flow only
    executorUnderlying,                   // writer flow only (your underlying source)
    executorSettlement,                   // writer: receives net premium; trader: pays premium
    treasurySettlement,                   // ATA(settlement_mint, treasury), init_if_needed
    position: positionKeypair.publicKey,
    nonceRecord: nonceRecordPda,          // PDA ["nonce", mm_account, nonce_le_u64]
    instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
    tokenProgram: TOKEN_PROGRAM_ID,
    associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
  })
  .instruction();

const tx = new Transaction().add(sigVerifyIx, executeIx);
// sigVerifyIx is instruction 0 → sig_ix_index = 0. If you prepend compute-
// budget instructions, sig_ix_index must be the precompile's actual index.
await sendAndConfirm(tx, [wallet, positionKeypair]);
```

Key points:

- **`sig_ix_index` must equal the precompile instruction's position in the
  transaction.** With `[sigVerifyIx, executeIx]` it is 0; with two
  compute-budget instructions first it is 2.
- The **`position` account is a fresh keypair** and must co-sign (Anchor
  `init` + `signer`).
- **Replay protection:** the `nonce_record` PDA
  (`["nonce", mm_account, nonce as u64 LE]`, options_core program) is
  `init`-ed by the instruction — a consumed nonce fails before the handler
  runs. Never reuse a quote.
- Execute **before `valid_until_ms`** (on-chain check + the service's
  reservation TTL).
- For **put buckets**, use `execute_put_write` — identical quote/flow/
  `sig_ix_index` semantics with the put-side accounts.
- The quote is bound to this deployment via
  `quote.protocol_id == Config PDA` (also served by solana-token-info's
  `/program-info` as `configPda`).
