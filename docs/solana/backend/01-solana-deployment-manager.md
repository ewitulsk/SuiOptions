# solana-deployment-manager (`tools/solana-deployment-manager`, bin `solana-deploy`)

Solana counterpart of `tools/deployment-manager` (bin `deploy`). Owns
**protocol initialization + the deployment registry** (`solana-deployments.json`).

## Divergence from the Sui tool (deliberate)

The Sui tool compiles and publishes Move packages because every publish mints a
new package identity that must be captured. On Solana, program ids are fixed by
their deploy keypairs and upgrades are `solana program deploy` against the same
id — a solved problem owned by the Anchor/Solana CLI (`solana-contracts/
scripts/deploy-devnet.sh`). Reimplementing BPF loader writes in Rust buys
nothing and risks bricking upgrade authority. So:

- **Program deploys**: stay with `anchor build` + `solana program deploy`
  (documented in the tool README; the tool never touches program binaries).
- **solana-deploy** does everything *after* the binaries exist:
  1. `initialize` options_core (Config + Treasury PDAs) — idempotent: if the
     Config account exists, verify `admin` matches and skip.
  2. Optionally create **test SPL mints** (`--deploy-tokens`): classic SPL
     Token (not Token-2022 — the programs use `anchor_spl::token`), decimals
     per the hardcoded table (TUSDC/6, TBTC/8, TSOL/9), **mint authority = the
     faucet pubkey** passed via `--faucet-authority` (the solana-gas-station
     sponsor key; see gas-station guide) with fallback to the payer. Mints an
     initial supply to the deployer for MM bootstrap.
  3. Rebuild the `token_info` catalog slot preserving existing `pythFeedId`s
     (same carry-forward discipline as the Sui tool).
  4. Upsert the env slot in `solana-deployments.json`, preserving other envs.

## CLI

```
solana-deploy
  -e, --env <dev|staging|prod>          # JSON slot
  -n, --network <devnet|testnet|mainnet-beta>
  --rpc <url>                           # override; else secrets rpc_url, else public
  -o, --output <solana-deployments.json>
  -s, --secrets <tools/solana-deployment-manager/config/secrets.toml>
  --core-program-id / --venue-program-id / --vault-program-id <pubkey>
                                        # default: read from solana-contracts/Anchor.toml
  --skip-init                           # don't call initialize
  --deploy-tokens                       # create test mints + seed supply
  --faucet-authority <pubkey>           # mint authority for test mints
  show                                  # print current slot (subcommand)
```

Secrets: `[solana]` per-network keypair (the **admin/deployer** key —
`config.admin`). Loaded via `runtime_config::Secrets::solana_keypair(network)`.

## Implementation notes

- Standalone workspace (needs `solana-sdk`, `solana-client`, `anchor-lang` via
  the program crates, `spl-token`, `spl-associated-token-account`).
- Depends on `options_core` program crate (`no-entrypoint`) for the
  `initialize` instruction encoding and Config account layout; PDA derivations
  via `Pubkey::find_program_address` with the seeds from `state.rs`.
- Own `json_store.rs` mirroring the Sui tool's (env-slot upsert, snake_case
  containers / camelCase fields, missing envs rendered `null`).
- Reads default program ids from `solana-contracts/Anchor.toml` so the common
  path needs no id flags.
- Idempotency: every step checks on-chain state first (Config exists? mint
  exists in the current file + on chain?) so re-runs converge instead of
  erroring — Solana has no "publish = new identity" so the tool must be
  re-runnable.
- Test-token table: `TUSDC/6`, `TBTC/8`, `TSOL/9` (drop TWAL/TDEEP — Sui-only
  assets; TSOL replaces TSUI as the gas-asset stand-in). Pyth feed ids seeded
  from the same feeds the Sui file uses for BTC/USDC plus SOL/USD.

## Verification

- `cargo test` unit tests: json_store round-trip (upsert preserves other envs,
  carry-forward of pythFeedId), PDA derivation against the program crates'
  seeds, idempotent-init planning (given existing Config → skip).
- Manual (documented, operator-run): against devnet — run `initialize` +
  `--deploy-tokens`, then `solana-deploy show` and `spl-token supply`.
