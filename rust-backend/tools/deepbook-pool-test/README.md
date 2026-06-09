# deepbook-pool-test

Proof-of-concept: create a **DeepBook v3 permissionless pool** that trades our
test tokens, on **Sui testnet**. Target pool: `Pool<TBTC, TUSDC>`.

This proves we can list our option-style coins on DeepBook as a trading venue.

## TL;DR

```bash
# from rust-backend/
cargo run -p deepbook-pool-test -- status     # readiness + balances
cargo run -p deepbook-pool-test -- run        # create + verify (needs >= 500 DEEP)
```

The signer key is read from `tools/deepbook-pool-test/config/secrets.toml`
(`[sui] testnet = "suiprivkey1..."`), which is gitignored.

## The one prerequisite: 500 DEEP

`create_permissionless_pool` charges a **500 DEEP** fee — the *real* DeepBook
testnet DEEP coin
(`0x36dbef86…::deep::DEEP`), **not** our TDEEP test token. This is verified
against the live deployed contract:

- `constants::pool_creation_fee()` dev-inspects to `500000000` (= 500 DEEP).
- A deployed-code dry-run of `create_permissionless_pool<TBTC,TUSDC>` with less
  DEEP aborts with code `1` at the fee check (and reaches it — so registry,
  type args, and tick/lot/min are all valid; no TBTC/TUSDC pool exists yet).

Acquiring 500 testnet DEEP is the hard part and is currently a **manual
prerequisite**:

- The DEEP coin package has **no faucet** (only `burn` / `total_supply`).
- DeepBook's testnet order books hold only **~20 DEEP** with no observed
  refills; DBUSDC's `mint` is `TreasuryCap`-gated (not ours).
- So the wallet must be funded with >= 500 DEEP out-of-band (Sui/DeepBook
  Discord, another holder, or a DEX with testnet DEEP depth).

Once the wallet holds >= 500 DEEP, `run`/`create` does the rest.

## Subcommands

| Command        | What it does |
|----------------|--------------|
| `status`       | Print address, SUI/DEEP/TBTC/TUSDC balances, fee, readiness. |
| `acquire-deep` | Best-effort SUI→DEEP via the whitelisted DEEP/SUI pool. Thin on testnet (~20 DEEP), so usually insufficient. |
| `create`       | Create `Pool<TBTC, TUSDC>` (requires >= 500 DEEP). Dry-run-gated. |
| `verify <id>`  | Read back a pool object and print its type. |
| `run`          | `status` → `create` (if ready) → `verify`. Default. |

Every on-chain submit is **dry-run first**, so a bad assumption fails without
spending.

## Key addresses (testnet)

| Thing | Value |
|---|---|
| DeepBook package | `0x22be4cade64bf2d02412c7e8d0e8beea2f78828b948118d46735315409371a3c` |
| Registry (shared) | `0x7c256edbda983a2cd6f946655f4bf3f00a41043993781f8674a7046e8c0e11d1` |
| DEEP/SUI pool (whitelisted) | `0x48c95963e9eac37a316b7ae04a0deb761bcdcc2b67912374d6036e7f0e9bae9f` |
| DEEP coin (6 dec) | `0x36dbef866a1d62bf7328989a10fb2f07d769f4ee587c0de4a0a256e57e0a58a8::deep::DEEP` |
| TBTC (base, 8 dec) | `0x159cc8d6…::tbtc::TBTC` |
| TUSDC (quote, 6 dec) | `0x159cc8d6…::tusdc::TUSDC` |

Pool params (tunable, in `src/main.rs`): `tick=10000 lot=1000 min=10000`
(lot/min are powers of 10 with `1000 ≤ lot ≤ min`).
