# deployment-manager

Compiles, publishes, and post-initializes the options-protocol Move package on Sui — purely from Rust, no shell-out to `sui` CLI. Writes every important on-chain address into one `deployments.json` keyed by network.

## What it does, per network

1. **Build** the Move package via `sui-move-build`.
2. **Publish** via `sui-sdk`'s transaction builder (auto-selects a gas coin). This uses `publish_upgradeable`, so an `UpgradeCap` is created and transferred to the deployer.
3. **Parse** `object_changes` for: `package_id`, `AdminCap`, `ProtocolConfig`, `UpgradeCap`.
4. **Post-init**: call `treasury::create_and_share(&AdminCap)` and capture the resulting `Treasury` shared-object ID.
5. **Merge** into `deployments.json`, replacing only the targeted network's entry. Other networks' records are untouched.

## Usage

```bash
# From the rust-backend/ directory.

# Deploy to all three networks (default).
SUI_PRIVATE_KEY=suiprivkey1... cargo run -p deployment-manager

# Deploy to one specific network.
SUI_PRIVATE_KEY=suiprivkey1... cargo run -p deployment-manager -- --network testnet

# Use per-network keys (each falls back to SUI_PRIVATE_KEY if unset).
SUI_PRIVATE_KEY_MAINNET=suiprivkey1... \
SUI_PRIVATE_KEY_TESTNET=suiprivkey1... \
SUI_PRIVATE_KEY_DEVNET=suiprivkey1...  \
cargo run -p deployment-manager
```

### Flags

| flag                | default                  | meaning                                                                  |
| ------------------- | ------------------------ | ------------------------------------------------------------------------ |
| `-n, --network`     | (all three)              | `mainnet` \| `testnet` \| `devnet`. Omit to deploy to all.               |
| `-c, --contracts`   | `../contracts`           | Path to the Move package (relative to where you `cargo run` from).       |
| `-o, --output`      | `deployments.json`       | Output JSON path. Read-merge-write semantics; other networks preserved.  |
| `--gas-budget`      | `500000000` (0.5 SUI)    | Gas budget in MIST per transaction (publish + init each consume one).    |
| `--skip-init`       | off                      | Publish only; skip `treasury::create_and_share`.                         |
| `--deploy-tokens`   | off                      | Also publish `test-tokens` (TUSDC/TBTC/TWAL/TDEEP) and record faucets.   |
| `--test-tokens`     | `../test-tokens`         | Path to the test-tokens Move package.                                    |

### Environment

| var                            | required             | notes                                                                                  |
| ------------------------------ | -------------------- | -------------------------------------------------------------------------------------- |
| `SUI_PRIVATE_KEY`              | fallback for any net | Standard `suiprivkey1...` bech32 string (the format `sui keytool export` emits).       |
| `SUI_PRIVATE_KEY_MAINNET`      | optional             | Overrides `SUI_PRIVATE_KEY` for mainnet. Same for `_TESTNET` and `_DEVNET`.            |
| `RUST_LOG`                     | optional             | Standard tracing filter. Default `info`.                                               |

## Output shape

```json
{
  "mainnet": null,
  "testnet": {
    "packageId":         "0x...",
    "adminCapId":        "0x...",
    "protocolConfigId":  "0x...",
    "upgradeCapId":      "0x...",
    "treasuryId":        "0x...",
    "publishDigest":     "8sQ...",
    "initDigest":        "Hd2...",
    "deployer":          "0x...",
    "deployedAt":        "2026-05-15T20:00:00+00:00",
    "network":           "testnet",
    "testTokens": {
      "packageId":      "0x...",
      "upgradeCapId":   "0x...",
      "publishDigest":  "...",
      "deployedAt":     "2026-05-15T20:00:05+00:00",
      "tokens": {
        "TBTC":  { "coinType": "0x<pkg>::tbtc::TBTC",   "faucetId": "0x...", "decimals": 8 },
        "TDEEP": { "coinType": "0x<pkg>::tdeep::TDEEP", "faucetId": "0x...", "decimals": 6 },
        "TUSDC": { "coinType": "0x<pkg>::tusdc::TUSDC", "faucetId": "0x...", "decimals": 6 },
        "TWAL":  { "coinType": "0x<pkg>::twal::TWAL",   "faucetId": "0x...", "decimals": 9 }
      }
    }
  },
  "devnet": null
}
```

The `testTokens` block is omitted entirely when `--deploy-tokens` was not passed. Each `--deploy-tokens` run publishes a fresh test-tokens package and overwrites the previous block for that network — old faucets are orphaned but cost nothing.

Every run preserves untouched networks (their existing entries stay; the targeted network is overwritten). Field names are camelCase to stay compatible with the TS reference and any frontend / indexer config already consuming that shape.

## Failure semantics

- Per-network deployments are independent. Mainnet failing won't roll back testnet.
- `deployments.json` is written after **each** successful network, so a later failure can't lose an earlier success.
- If any network fails, the process exits non-zero and prints which ones.

## Why pure Rust (not shell out to `sui client publish`)

- Self-contained — no version skew between the manager and whichever `sui` CLI happens to be on PATH.
- Better error surface — typed `ObjectChange` matching beats grep-on-JSON.
- Single binary suitable for CI; no extra toolchain.

Trade-off: first build is slow because the Sui Rust workspace is a heavy git dep. Cached afterwards.

## Adding more post-init calls

If the protocol grows post-publish initialization (e.g., create initial buckets, set fees), extend `deploy.rs::create_and_share_treasury` into a generic `run_post_init(...)` that bundles multiple `move_call`s into a single PTB, then surface the resulting object IDs through `NetworkDeployment`.
