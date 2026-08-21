# Dakota sandbox — verified behaviour

Findings from probing `https://api.platform.sandbox.dakota.xyz` live with our sandbox API key
(2026-08-02). These supersede the prose docs wherever they disagree — several documented
shapes are wrong or incomplete.

Our sandbox client id is `3HN0RQshF6yCiMXxhCD7yIJarU9` ("Pismo Protocol").

## Auth and conventions

- `x-api-key: <key>` on every request. `x-idempotency-key: <uuid>` on every **POST** — omitting
  it is a `400`. Do **not** send it on GET/PUT/PATCH/DELETE.
- Errors are RFC 9457 Problem Details: `{type, title, status, detail, instance, request_id}`,
  plus an `errors[]` array of `{field, message, code}` on validation failures.
- Ids are KSUIDs (27 chars).

## What does NOT exist

- **No token-issuance API.** Nothing creates a stablecoin. Our "supported assets" catalog is
  ours to own.
- **No assets endpoint.** The only capability routes are `/capabilities/networks` and
  `/capabilities/countries`. `/info/networks` is a `404` — the docs' path is wrong.
- **`GET /self-serve/credits/pricing` → `403`**: *"Credit management is only available for
  self-serve customers."* We are not a self-serve client, so there is **no fee-schedule
  endpoint available to us**. Rates must be admin-entered.
- `GET /wallets` → `405`. The collection is POST-only; there is no list-wallets route.

## Other traps found by running it

- **`GET /events?limit` caps at 100.** Asking for more is a `400`, not a silent clamp.
- **Receipts come in two shapes.** `GET /auto-transactions` nests them
  (`{"output":{"amount":"2","asset":"USDC"}}`); `GET /events` and webhook deliveries flatten
  them (`{"outgoing_amount":"2","output_currency":"USDC"}`), and there `dakota_fee` is a bare
  decimal string rather than an object. Handling only the nested form leaves every
  webhook-sourced ledger row with a NULL amount.
- **Events name the account, not the customer.** There is no `customer_id` on an event object —
  only `auto_account_id`. Attribution has to come from your own account→customer mapping, or
  every per-customer total stays empty.
- **`simulate/onboarding` does not push a status update you can rely on in the same breath.**
  The simulation returns `approved`, but a local copy of `kyb_status` only catches up when the
  webhook lands. Anything that gates on the local status (as `POST /accounts` does) must
  re-read `GET /customers/{id}` right after simulating, or the next call is still refused as
  `pending`.
- **Postgres widens `SUM(bigint)` to `NUMERIC`.** Unrelated to Dakota, but it broke the flow
  aggregation until the SUMs were cast back with `::bigint`.

## Where rates actually come from

Not from a pricing endpoint — from **transaction receipts**. Every auto-transaction carries:

```json
"receipt": { "input": {"amount":"2","asset":"USD"}, "output": {"amount":"2","asset":"USDC"},
             "exchange_rate": "1", "dakota_fee": {"amount":"0","asset":"USD"},
             "client_fee": {...}, "external_fee": {...} }
```

So the rates view is: admin-entered expected schedule + realised `exchange_rate`/fee history
derived from completed transactions.

## `GET /capabilities/networks` (verified)

```
arbitrum-mainnet, arbitrum-sepolia, base-mainnet, base-sepolia, ethereum-goerli,
ethereum-holesky, ethereum-mainnet, ethereum-sepolia, evm, optimism-mainnet,
optimism-sepolia, polygon-amoy, polygon-mainnet, solana-devnet, solana-testnet,
solana-mainnet
```

Mainnets are **listed but rejected** by object-create endpoints in sandbox. `evm` is a
wildcard valid in all environments.

## Onboarding state machine — the gate that matters

`POST /accounts` fails with `"Customer is not KYB-approved by Dakota"` until the customer is
approved. Getting there in sandbox:

```
POST /sandbox/simulate/onboarding
{ "type": "kyb_approve", "applicant_id": "<application_id>", "simulation_id": "<unique>" }
```

**`kyb_approve` is the master transition — use it for individuals too.** Confirmed traps:

- The body needs `type`, `applicant_id`, `simulation_id`. `applicant_id` is the
  **`application_id`**, not the customer id. There is no `customer_id`/`target_status` field
  (the docs' example is wrong).
- `kyc_approve` on a fresh individual is a **no-op** (`not_started → not_started`). Only
  `kyb_approve` advances it. After `kyb_approve` the customer shows `kyb_status: "active"`
  while `kyc_status` stays `"pending"` — and that is sufficient for `/accounts`.
- `applicant_activate` is idempotent once approved.

Customer status fields: `kyb_status`, `kyc_status`, `application_status`, plus `rd_allowed`.

## Three-tier hierarchy (verified working)

```
POST /customers {"name","customer_type":"business","is_sub_client":true}   → sub-client
POST /customers {"name","customer_type":"individual","sub_client_id":"<sub>"} → its customer
```

`GET /customers/sub-client-summary` → `[{sub_client_id, sub_client_name, customer_count}]`.

`POST /customers` returns `application_url` (hosted form, embedded token) and
`application_expires_at` — **nanoseconds**, not seconds, unlike every other timestamp.

`GET /customers/{id}/capabilities` returns per-capability `requirements[]` with
`{key, severity, title, type, url}` — ideal for a "what's needed to unlock" panel.

## Ramp flow (verified end-to-end)

1. `POST /customers/{id}/recipients` — `{name}`. Address optional for crypto-only; **required
   before adding any fiat destination**.
2. `POST /recipients/{id}/destinations` — discriminated by `destination_type`:
   `crypto` / `fiat_us` / `fiat_iban`. Crypto needs `{name, crypto_address, network_id}`.
3. `POST /accounts`:
   - **onramp** — `capabilities` is **required** (`["ach","fedwire"]`); undocumented as
     required, fails `400 "capabilities are required"` without it. Returns a full
     `bank_account` (Lead Bank, ABA + account number).
   - **swap** — returns `source_crypto_address` on the source network.
   - **offramp** — needs `fiat_destination_id`, so the recipient must have an address.

Verified onramp: `$2.00 USD → 2 USDC` on `base-sepolia`, status `processing`.

## Sandbox limits

- **$2.00 cap per transaction.** `5.00` → `400 "amount 5 exceeds sandbox cap of 2"`.
- USDT unsupported. USD, USDC and RD treated 1:1.
- `POST /sandbox/simulate/inbound` needs `{simulation_id, type, amount, currency}` plus
  `account_id` (ACH/Fedwire/FedNow inbound) or `wallet_address` (`crypto_inbound`).

## Wallets — supported in sandbox

Full chain verified. Wallet `0xF2e1556b5b41e71244685C6e64e5Dc6C64e1d62B` created.

```
POST /signers        {name, public_key, key_type:"ES256"}   # base64 DER SPKI (X.509 PKIX)
POST /signer-groups  {name, member_keys:[<public_key>]}     # public keys, NOT signer ids
POST /policies       {name, description, signer_group_id, rules:[...]}
POST /wallets        {name, family:"evm"|"solana", signer_groups:[id], policies:[id]}
GET  /wallets/{id}/balances → {address, balances[], total_amount_usd}
```

Quirks: `key_type` echoes back as `KEY_TYPE_ES256`, not `ES256`. `POST /policies` accepts
`signer_group_id` but **returns it as `null`** — attach via the wallet instead.

### Endorsed (signed) requests — broader than documented

**Nine** endpoints take an `EndorsedRequest` (`{signatures:[base64], intent:{...}}`), not just
transactions:

```
POST   /wallets/{id}/transactions          PUT    /policies/{pid}/wallets/{wid}
POST   /policies/{pid}/rules               DELETE /policies/{pid}/wallets/{wid}
PATCH  /policies/{pid}/rules/{rid}         DELETE /policies/{pid}
DELETE /policies/{pid}/rules/{rid}
PUT    /wallets/{wid}/signer-groups/{gid}  DELETE /wallets/{wid}/signer-groups/{gid}
```

Signing: **RFC 8785 JCS canonicalize → SHA-256 → ECDSA P-256 → ASN.1 DER → base64**.
`snake_case` keys, amounts as strings, unset fields omitted. Browser `crypto.subtle` returns
IEEE P1363 `r||s` and must be converted to DER.

### Two undocumented rules that both fail as `endorsement validation failed`

That error is the *only* feedback you get, and it names nothing. Both of these cost real
debugging time and are now covered by tests in `services/dakota-service/src/wallet/`.

**1. Amounts must be normalized before signing.** Dakota normalizes the decimal before
rebuilding the intent it verifies against, so a signature over `"1.00"` is checked against
`"1"`. Measured against the live sandbox with one key and one wallet, varying only the amount:

| amount sent | result |
|---|---|
| `"1"` | accepted → *Insufficient balance… Required: 1 USDC* |
| `"1.00"` | **endorsement validation failed** |
| `"0.50"` | **endorsement validation failed** |
| `"0.01"` | accepted → *Insufficient balance… Required: 0.01 USDC* |

Strip trailing zeros from the fraction and drop the point if nothing remains:
`"1.00"` → `"1"`, `"0.50"` → `"0.5"`, `"0.01"` unchanged. That is `wallet::normalize_amount`.
Every whole-dollar transfer a person types would otherwise be rejected.

**2. Transmit the canonical form, not the struct.** A Rust struct serializes in *declaration*
order, so posting one sends key order that differs from the canonical bytes that were signed.
`serde_json::Value` orders its keys, so `endorse()` returns the intent as a `Value` rebuilt
from the canonical bytes — the wire form then equals the signed form by construction.

A useful diagnostic property: an **insufficient-balance** rejection is *success* for signing
purposes. It means the signature verified and Dakota reached policy evaluation. That is what
the `live_signature_is_accepted_by_dakota` test asserts on.

## PII exposure — why we store almost nothing

Dakota responses are full of PII. Confirmed in live responses:

- `GET /customers` → `email`, `name`
- `POST /accounts` (onramp) → `bank_account.account_holder_name`, `account_number`,
  `aba_routing_number`
- `GET /events` → `sender_details.sender_account_holder_name`, `sender_account_number`

**Therefore: never persist a Dakota response body.** Extract only ids, enums, amounts, assets
and timestamps. Proxy everything else straight to the browser.

## Probe artifacts left in sandbox

| Kind | Id |
|---|---|
| signer | `3HNC7kMf188HuSKgFWXqKJreTqv` |
| signer group | `3HNC8vt3NOat7GWDRJgeVe27Kru` |
| policy | `3HNC8wVOBg2KRTVWl50owNTH3i2` |
| wallet (evm) | `3HNC95HOlmEHtkb8iGQt9WScIvG` / `0xF2e1556b…` |
| sub-client | `3HNCB4vp2zWMwdfoY33qKK11iOJ` "Acme Partner Bank" |
| individual | `3HNCB1zUMQe4bUmiYHPw6xMPcOr` "Jane Probe" |
| onramp account | `3HNCN914HGh2Sr95XpcJBgMPLAT` |
| swap account | `3HNCNG7l9WZzwGSjlLlWBzccg4v` |

The probe's P-256 private key was scratchpad-only and is **not** the treasury key — Phase 4
generates its own into Secrets Manager.
