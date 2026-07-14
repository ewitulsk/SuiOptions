# solana-balance-monitor (`services/solana-balance-monitor`)

Watches the **SOL balances** of the Solana operational wallets. **Standalone
workspace** (needs keypair decode to derive addresses from secrets files —
same loose-coupling trick as the Sui monitor). Ops port 9012 (own container).

## Why a separate service (not new [[watch]] kinds in balance-monitor)

balance-monitor lives in the Sui workspace and links `sui-tx`/`sui-sdk`;
adding Solana keypair/RPC support would violate the workspace isolation rule.
A clone is ~300 lines and keeps the alert pipeline uniform.

## Behavior

- Poll loop (`poll_interval_secs` 60): for each `[[watch]]`, resolve the
  address (from `secrets_file` → `[solana]` keypair pubkey, or explicit
  `address`), `getBalance` via RPC, export gauges
  `sol_balance_sol{service,address}` and `sol_balance_low{service}`, and
  while below threshold emit
  `error!(alert_id = "low-balance-<service>", …)` — service names are already
  `solana-*` so alert ids stay unique against the Sui monitor's.
- Watches (prod): `solana-gas-station` (fee payer, threshold 5 SOL),
  `solana-scheduler` (2), `solana-keeper` (2), `solana-mm-bot` (2, optional —
  skipped if secret absent).
- `ALERT_TEST=1` canonical test alert, as Sui.

## Config / secrets

- `environment`, `network`, `ops_addr 0.0.0.0:9012`, `poll_interval_secs`,
  `[[watch]] { name, secrets_file | address, low_balance_sol }`.
- Optional `--secrets` for the shared `[solana] rpc_url` override; public RPC
  fallback.
- Reads sibling services' rendered secrets from `/run/secrets/…` (compose
  mounts the same secrets dir read-only).

## Verification

- Unit: config validation (exactly one of secrets_file/address), lamports→SOL
  conversion, threshold hysteresis behavior (alert every poll while low —
  same as Sui).
