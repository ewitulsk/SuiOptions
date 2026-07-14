# solana-deployment-manager (`solana-deploy`)

Solana counterpart of `tools/deployment-manager`. Owns **protocol
initialization + the deployment registry** (`solana-deployments.json`) —
it never touches program binaries. Program deploys/upgrades stay with the
Anchor/Solana CLI (`solana-contracts/scripts/deploy-devnet.sh`): program
ids are fixed by their deploy keypairs, so there is no publish step to
capture here.

## What it does, per run (idempotent — re-runs converge)

1. Resolve program ids (`--*-program-id` flags, else
   `solana-contracts/Anchor.toml`).
2. `initialize` options_core (Config + Treasury PDAs). If the Config PDA
   already exists it verifies `admin` matches the loaded keypair and skips.
3. `--deploy-tokens`: create the test SPL mints (TUSDC/6, TBTC/8, TSOL/9;
   classic SPL Token, no freeze authority). Mints already recorded for
   this env that still exist on-chain with the right decimals are kept.
   New mints get 1e6 whole tokens minted to the deployer, then the mint
   authority is handed to `--faucet-authority` (the solana-gas-station
   faucet key; defaults to the deployer). Refused on mainnet-beta.
4. Rebuild the `token_info` catalog, preserving existing `pythFeedId`s
   (fresh entries are seeded with the BTC/USDC/SOL feed ids).
5. Upsert the env slot in `solana-deployments.json`; other envs untouched,
   un-deployed envs rendered `null`.

## Usage

Standalone cargo workspace — run from `rust-backend/` (the flag defaults
assume it):

```bash
cd rust-backend

# First-time setup: put the admin keypair in the secrets file.
cp tools/solana-deployment-manager/config/secrets.example.toml \
   tools/solana-deployment-manager/config/secrets.toml   # then edit

# Initialize + test mints on devnet, recorded under the staging slot.
cargo run --manifest-path tools/solana-deployment-manager/Cargo.toml -- \
  -e staging -n devnet --deploy-tokens --faucet-authority <FAUCET_PUBKEY>

# Print the current slot.
cargo run --manifest-path tools/solana-deployment-manager/Cargo.toml -- \
  -e staging show
```

## Operator flow

1. `anchor build` + deploy the programs (`solana-contracts/scripts/deploy-devnet.sh`).
2. Run `solana-deploy` (above) to initialize and write `solana-deployments.json`.
3. Commit the updated `solana-deployments.json`.
4. Redeploy **solana-token-info** (the file's only reader; every other
   service reads through it).

## Flags

| flag | default | meaning |
| --- | --- | --- |
| `-e, --env` | (required) | JSON slot: `dev` / `staging` / `prod`. |
| `-n, --network` | (required) | `devnet` \| `testnet` \| `mainnet-beta`. |
| `--rpc` | secrets `solana.rpc_url`, else public | JSON-RPC override. |
| `-o, --output` | `solana-deployments.json` | Registry path (rust-backend root). |
| `-s, --secrets` | `tools/solana-deployment-manager/config/secrets.toml` | `[solana]` admin keypair. |
| `--contracts` | `../solana-contracts` | Where to find `Anchor.toml`. |
| `--core-program-id` etc. | from `Anchor.toml` | Per-program id overrides. |
| `--skip-init` | off | Record ids only; don't call `initialize`. |
| `--deploy-tokens` | off | Create/refresh the test mints. |
| `--faucet-authority` | the deployer | Final mint authority for test mints. |
| `show` | — | Subcommand: print the `-e` slot (or the whole file). |

## Verification

`cargo test` covers: json round-trip through the reader crate
(`crates/solana-deployments`), env upsert preservation, pythFeedId
carry-forward, PDA derivations against the program crate's seeds,
idempotent-init/token planning, and a LiteSVM end-to-end test against the
real `options_core.so` (skipped when not built). Manual, against devnet:
run with `--deploy-tokens`, then `solana-deploy show` and
`spl-token supply <mint>`.
