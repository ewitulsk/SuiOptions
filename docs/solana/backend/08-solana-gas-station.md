# solana-gas-station (`services/solana-gas-station`)

Sponsors users' transaction fees on Solana. **Standalone workspace**
(solana-sdk). Port 9009.

## The Solana sponsorship model (design decision)

**Chosen: self-hosted fee-payer co-signing** — the pattern the ecosystem
standardized (Octane, now the Solana-Foundation-shipped **Kora** node): a
Solana transaction's first account is the fee payer; whoever signs that slot
pays. The service holds a hot fee-payer keypair, validates a user-built
transaction against a template allow-list, signs the fee-payer slot, and
returns the signature; the wallet adds the user signature and the frontend
submits. This is a 1:1 conceptual match with our Sui gas station (validate →
attach gas → co-sign → client submits), so the service architecture ports
directly.

Why not the alternatives:
- **Helius**: provides RPC/Sender/LaserStream but no turnkey sponsorship API —
  their own guidance is "run a fee payer wallet". We still use Helius as the
  submission/read RPC.
- **Kora sidecar**: Foundation-audited and KMS-ready, but (a) its core feature
  is charging fees in SPL tokens, which we don't want (we sponsor free), and
  (b) its policy surface is a program allow-list — coarser than our
  per-instruction template governance that stays in lockstep with the
  frontend. Noted as the future path if we want KMS/TEE signing or paid
  sponsorship.

Failure points (documented for ops):
- Hot key = drainable balance. Mitigations: per-transaction **lamport-delta
  cap** (see validation), `min_balance_threshold` health, solana-balance-monitor
  alerting, small standing balance.
- **Rent sponsorship**: ATA creation debits the fee payer ~0.002 SOL when the
  station is the payer of `create_associated_token_account`. Bounded by the
  same lamport-delta cap.
- Blockhash expiry (~60–90 s) between sponsor-sign and wallet-sign+submit:
  the flow keeps the station **last signer chronologically? No — fee payer
  signs the final message**, so the client must build with a fresh blockhash
  and submit promptly; on expiry the frontend simply rebuilds and re-requests
  (sponsorship is stateless and free to retry).
- No auth / no rate limiting — parity with the Sui station (template
  allow-list is the abuse boundary). Flag: on Solana the station also fronts
  the faucet, which mints valueless test tokens; still capped per request.

## HTTP surface

- `GET /health` → "ok".
- `GET /balance` → `{ address, balance_lamports: "…", balance_sol: f64,
  threshold_lamports: "…", healthy }`.
- `POST /sponsor` — request:
  `{ "transaction": "<base64 serialized VersionedTransaction (unsigned or user-signed), fee payer = station pubkey>" }`
  response:
  `{ "transaction": "<base64, station signature applied>",
     "sponsor_pubkey": "…", "sponsor_signature": "<base58>" }`
  Errors: 400 undecodable, 422 policy refusal (with a template-mismatch
  description, like `describe_ptb`), 503 low balance.
  The client may send the transaction *before* user signing (station signs
  first, wallet second) or after — signature order is irrelevant on Solana;
  the frontend flow is: build with `feePayer = sponsorPubkey` + fresh
  blockhash → POST /sponsor → wallet `signTransaction` → send raw.
- `POST /faucet` (non-mainnet only) — `{ "recipient": base58, "ticker": "TBTC" }`
  → creates the recipient's ATA if missing, mints the configured per-request
  amount, submits, returns `{ signature }`. The station's key is the test
  mints' mint authority (set by solana-deploy `--faucet-authority`). Replaces
  the Sui on-chain faucet flow (no faucet program exists on the Solana side);
  design decision logged in 00-architecture.

## Validation (`template.rs` port — the security core)

A `TxTemplate` pins, per named flow: `required` (ordered subsequence of
(program_id, ix_discriminator) pairs), `allowed` (closed set), and structural
guards. Matching walks the message's compiled instructions:

- **Benign set** (skipped like Sui's SplitCoins/MergeCoins): ComputeBudget
  program instructions, `spl-associated-token-account` create /
  create-idempotent, system-program `advance_nonce`? (no — nonce txs
  rejected v1), memo.
- Every remaining instruction must target an allowed program with an allowed
  8-byte Anchor discriminator (options_core / auction_venue / options_vault /
  spl-token transfer+approve as required by flows) — and for the quote flows,
  the **Ed25519SigVerify precompile** instruction (program
  `Ed25519SigVerify111…`) is allowed.
- **Fee-payer safety guards** (Solana-specific, replaces Sui's dry-run-owner
  model):
  1. Station pubkey may appear ONLY as the fee payer (account 0). If it
     appears in any instruction's account list or as a writable account
     elsewhere → refuse.
  2. No `system_program::transfer`/`create_account`/`assign` where any signer
     is the station key (covered by 1, kept explicit).
  3. Simulate via RPC (`simulateTransaction`, sigVerify off) and compute the
     station's **lamport delta**; refuse if `fee + rent debits >
     max_sponsor_lamports_per_tx` (default 5_000_000 = 0.005 SOL).
  4. Reject `AddressLookupTable`-bearing v0 transactions in v1 (lookup tables
     can smuggle accounts past static inspection); legacy + v0-without-LUT ok.
- Templates built at boot from the solana-token-info snapshot, mirroring
  frontend flows: `write` / `buy` (execute_write + ed25519 precompile),
  `exercise`, `redeem`, put twins, vault `deposit` / `claim_shares` /
  `initiate_withdraw` / `complete_withdraw` / `instant_withdraw_pending`,
  venue `bid`.

## Config / secrets

- `environment`, `bind_addr 0.0.0.0:9009`, `network`, `allowed_origins`,
  `min_balance_threshold_lamports` (1 SOL), `max_sponsor_lamports_per_tx`
  (5e6), `faucet_amounts` per ticker + `faucet_enabled` (auto-false on
  mainnet-beta), `token_info_url`.
- Secrets `options/<env>/solana-gas-station`: `[solana]` keypair (fee payer =
  faucet authority) + shared `solana-rpc` override rendering.
- Metrics: `solana_gas_station_sponsorships_total{outcome}`,
  `..._sponsor_lamports` histogram, `..._faucet_mints_total`.

## Verification

- Unit: template matcher (accept/reject fixtures per flow built with the
  program crates' ix encoders), fee-payer-position guard, lamport-cap logic
  (simulated result fixture), LUT rejection.
- Integration: litesvm- or bankrun-style local validation is heavy; instead
  golden serialized-transaction fixtures generated by a small builder test.
