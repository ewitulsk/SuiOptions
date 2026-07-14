# solana-token-info — Frontend Integration Guide

Single source of truth for the Solana supported-token catalog and the
deployed programs' on-chain ids. The frontend never reads
`solana-deployments.json` — everything comes from this API.

## Base URL

```
https://<host>/{env}/solana-token-info
```

e.g. `https://sui-options.com/staging/solana-token-info`. All examples below
are relative to that base.

## Token identity: the mint address

A token's id is its SPL **mint address** — a base58 string that decodes to 32
bytes. Comparison is **byte-exact**: no normalization, no case-folding, no
prefixes. Store and compare mints as the exact strings this API returns.
Write endpoints reject anything that doesn't base58-decode to 32 bytes with
`400`.

## Endpoints

### `GET /health`

`200` with body `ok`.

### `GET /tokens`

Full catalog. Optional `?enabled=true` filters to enabled tokens. Sorted by
ticker.

```json
[
  {
    "mint": "So11111111111111111111111111111111111111112",
    "ticker": "TBTC",
    "name": "Test Bitcoin",
    "logo_uri": "https://assets.coingecko.com/coins/images/1/small/bitcoin.png",
    "decimals": 8,
    "pyth_feed_id": "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
    "enabled": true
  }
]
```

`logo_uri` and `pyth_feed_id` may be `null`.

**Overlay behavior:** on non-mainnet-beta networks (staging/prod are devnet
today), the deployment's test tokens (TBTC/TUSDC/TSOL) appear in this list
*without any DB row backing them* — they are derived from the deployment
record at read time. If an operator later creates a DB row for the same mint,
the DB row wins (the overlay entry disappears). Frontend takeaway: treat
`/tokens` as the complete list; don't special-case test tokens.

### `GET /tokens/:mint`

One token by mint (byte-exact). `404` if unknown.

```
GET /tokens/So11111111111111111111111111111111111111112
```

Response: one object with the same shape as a `/tokens` entry.

### `GET /program-info`

The deployment record for this environment, verbatim. Fetch once at app boot.

```json
{
  "optionsCoreProgramId": "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t",
  "auctionVenueProgramId": "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk",
  "optionsVaultProgramId": "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe",
  "configPda": "…",
  "treasuryPda": "…",
  "admin": "…",
  "network": "devnet",
  "deployedAt": "2026-07-11T00:00:00Z",
  "initializeSignature": "…",
  "testTokens": {
    "TBTC": { "mint": "…", "decimals": 8, "mintAuthority": "…" }
  }
}
```

What the frontend needs from it:

| Field | Use |
|---|---|
| `optionsCoreProgramId` / `auctionVenueProgramId` / `optionsVaultProgramId` | Program ids for building instructions and deriving PDAs. |
| `configPda` | The options_core Config PDA — this is the **quote `protocol_id`**; include it when requesting/submitting quotes. |
| `treasuryPda` | Fee/treasury account for instructions that pay fees. |
| `admin` | Config admin pubkey (display/ops only). |
| `network` | Cluster to point the wallet/RPC at: `devnet` \| `testnet` \| `mainnet-beta`. |
| `testTokens` | Faucet-only info (mint + mint authority). Absent on mainnet-beta. For decimals/feeds/display, use `/tokens` — not this block. |

All ids are base58 strings.

## Admin mutations (JWT-gated)

`POST /tokens`, `PUT /tokens/:mint`, `DELETE /tokens/:mint` require an admin
JWT from **solana-auth-service**: log in at
`/{env}/solana-auth/login` and send the returned token as
`Authorization: Bearer <jwt>`. `401` without it.

### `POST /tokens` — create/replace

```json
{
  "mint": "So11111111111111111111111111111111111111112",
  "ticker": "TBTC",
  "name": "Test Bitcoin",
  "logo_uri": "https://…",          // optional
  "decimals": 8,
  "pyth_feed_id": "e62d…5b43",      // optional, 64-hex
  "enabled": true                    // optional, default true
}
```

`200` with the stored token. `400` if `mint` is not a valid 32-byte base58
pubkey.

### `PUT /tokens/:mint` — update

Same body as POST; the `mint` in the path wins over any value in the body.
Upsert semantics (creates if absent). `400` on invalid path mint.

### `DELETE /tokens/:mint`

`204` on success, `404` if there is no DB row. Note: overlay test tokens have
no DB row, so they cannot be deleted — disable them by creating a DB row for
the same mint with `"enabled": false`.
