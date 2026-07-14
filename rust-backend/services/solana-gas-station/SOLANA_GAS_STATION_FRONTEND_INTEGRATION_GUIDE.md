# solana-gas-station — Frontend Integration Guide

The gas station sponsors your users' transaction fees on Solana. It holds a
hot fee-payer keypair, validates a user-built `VersionedTransaction` against
a template allow-list, co-signs the fee-payer slot, and returns the
transaction; the wallet adds the user signature and the frontend submits.

Base URL (nginx): `https://<host>/{env}/solana-gas-station` — port 9009
inside the docker network. Local dev: `http://127.0.0.1:9009`.

## Endpoints

| Method | Path       | Purpose |
|--------|------------|---------|
| GET    | `/health`  | liveness ("ok") |
| GET    | `/balance` | station pubkey + balance + health flag |
| POST   | `/sponsor` | validate + co-sign a fee-payer transaction |
| POST   | `/faucet`  | mint test tokens (non-mainnet only) |

## Sponsorship flow

1. **Fetch the sponsor pubkey** (cache it; it changes only on key rotation):

   ```ts
   const { address, healthy } = await fetch(`${GAS}/balance`).then(r => r.json());
   // healthy === false → default the "sponsored" toggle off and fall back
   // to user-paid fees.
   ```

2. **Build the transaction with the station as fee payer and a FRESH
   blockhash**:

   ```ts
   const { blockhash } = await connection.getLatestBlockhash("confirmed");
   const msg = new TransactionMessage({
     payerKey: new PublicKey(address),   // ← sponsor pubkey from /balance
     recentBlockhash: blockhash,
     instructions,                        // exactly one sponsored flow, see Templates
   }).compileToLegacyMessage();           // or compileToV0Message([]) — NO lookup tables
   const tx = new VersionedTransaction(msg);
   ```

   Legacy and v0 messages are both accepted. **v0 with address lookup
   tables is refused** (422 "lookup tables unsupported") — LUT-resolved
   accounts can't be statically inspected.

3. **POST `/sponsor`**:

   ```ts
   const body = { transaction: Buffer.from(tx.serialize()).toString("base64") };
   const resp = await fetch(`${GAS}/sponsor`, {
     method: "POST",
     headers: { "content-type": "application/json" },
     body: JSON.stringify(body),
   });
   // 200 → { transaction, sponsor_pubkey, sponsor_signature }
   const { transaction } = await resp.json();
   ```

   The station deserializes, checks the fee payer is its key, checks its key
   appears nowhere else in the message, matches the instruction sequence
   against the sponsored templates, **simulates** the transaction (it must
   succeed, and the station's lamport delta must stay under the per-tx cap),
   then signs.

4. **Wallet-sign the RETURNED transaction and submit raw**:

   ```ts
   const sponsored = VersionedTransaction.deserialize(
     Buffer.from(transaction, "base64"),
   );
   const signed = await wallet.signTransaction(sponsored); // adds the user sig
   const signature = await connection.sendRawTransaction(signed.serialize());
   await connection.confirmTransaction({ signature, blockhash, lastValidBlockHeight });
   ```

   Sign the **returned** bytes, not your local copy — the station's
   signature covers the exact message it received, and any local mutation
   invalidates it. Signature order doesn't matter on Solana; you may also
   wallet-sign before POSTing (the station preserves existing signatures).

### Blockhash freshness / retry on expiry

A blockhash is valid ~60–90 s. The sponsor round-trip plus the wallet prompt
eats into that window, so:

- fetch the blockhash immediately before building;
- submit promptly after the wallet signs;
- on `blockhash not found` / expiry at submission, simply **rebuild with a
  fresh blockhash and re-request sponsorship** — sponsorship is stateless
  and free to retry. Do not reuse the old sponsor signature; it covers the
  old message.

## Error semantics

| Status | Meaning | Retry? |
|--------|---------|--------|
| 400 | body isn't a decodable base64 `VersionedTransaction` | no — fix the encoding |
| 422 | **policy refusal** — wrong fee payer, station key referenced by an instruction, lookup tables, template mismatch, simulation failure, or lamport cap exceeded. The body says which; template mismatches include a human-readable instruction dump to diff against your builder. | **no — permanent for that transaction shape.** Rebuilding the identical shape yields the identical refusal. Only a template update on the station (redeploy) changes the answer. |
| 502 | RPC upstream failure during simulation/submission | yes, transient |
| 503 | station balance below its health threshold | fall back to user-paid fees; alerting is already firing |

## Templates — the lockstep warning

The station only sponsors the **exact instruction sequences** listed below
(compute-budget instructions, ATA `create`/`createIdempotent`, and memos may
ride along freely). **Any new frontend flow — or any change to an existing
flow's instruction sequence — needs a matching template in
`src/template.rs` (`protocol_templates`) or it will not be sponsored** and
users will see 422s. This is the same discipline as the Sui gas station's
`sui-tx template.rs`: ship the template change with (or before) the frontend
change.

Current templates (program ids from solana-token-info at boot):

| Template | Shape |
|----------|-------|
| `write/buy` | `options_core::execute_write` + Ed25519SigVerify precompile (the MM quote signature). The precompile is allowed **only** on the quote flows. |
| `exercise` | `options_core::exercise` |
| `redeem` | `options_core::redeem_position` |
| `put_write/put_buy` | `options_core::execute_put_write` + Ed25519SigVerify |
| `put_exercise` | `options_core::exercise_put` |
| `put_redeem` | `options_core::redeem_put_position` |
| `venue:bid` | `auction_venue::bid` |
| `vault:deposit` | `options_vault::deposit` |
| `vault:claim_shares` | `options_vault::claim_shares` |
| `vault:initiate_withdraw` | `options_vault::initiate_withdraw` |
| `vault:complete_withdraw` | `options_vault::complete_withdraw` |
| `vault:instant_withdraw_pending` | `options_vault::instant_withdraw_pending` |

One flow per transaction — don't batch two protocol instructions into one
transaction; it will match no template. Raw `spl-token` instructions
(transfer/approve/…) are never sponsored; the programs move tokens via CPI
under the user's own signature.

## Faucet

Non-mainnet only (force-disabled at boot on mainnet-beta). The station key
is the test mints' mint authority; it creates the recipient's ATA if
missing, mints the configured per-request amount, and submits the
transaction itself — no wallet interaction needed:

```ts
const resp = await fetch(`${GAS}/faucet`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ recipient: wallet.publicKey.toBase58(), ticker: "TBTC" }),
});
// 200 → { signature }  — already submitted and confirmed
```

Errors: 400 bad recipient, 403 faucet disabled on this deployment,
404 unknown ticker (body lists the available ones), 422 ticker has no
configured mint amount, 503 the station key isn't that mint's authority,
502 submission failure.

Amounts are fixed server-side per ticker (`faucet_amounts` config, raw
units) — there is no amount parameter.
