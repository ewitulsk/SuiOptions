//! DeepBook v3 permissionless-pool test framework (TESTNET ONLY).
//!
//! Goal: prove we can stand up a DeepBook permissionless pool that trades our
//! own test tokens — here `Pool<TBTC, TUSDC>` on Sui testnet.
//!
//! The one external cost is the **500 DEEP pool-creation fee** — the *real*
//! DeepBook testnet DEEP coin, not our TDEEP test token. This is verified live:
//! `constants::pool_creation_fee()` returns `500_000_000` and a deployed-code
//! dry-run aborts on the fee check (code 1) with anything less. There is no
//! public faucet for testnet DEEP and DeepBook's testnet order books are thin
//! (~20 DEEP, no refills), so acquiring 500 DEEP is a manual prerequisite. Once
//! the wallet holds >= 500 DEEP, `create` / `run` mints the pool.
//!
//! Subcommands:
//!   status        Print address, balances, fee, and readiness (default = run).
//!   acquire-deep  Best-effort SUI -> DEEP via the whitelisted DEEP/SUI pool.
//!   create        Create Pool<TBTC, TUSDC> (requires >= 500 DEEP). Dry-run first.
//!   verify <id>   Read back a pool object and print its parsed config.
//!   run           status -> create (if ready) -> verify.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use shared_crypto::intent::Intent;
use sui_tx::chain::{created_objects, ChainClient, ExecutedTransaction};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, Command, ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION, SUI_FRAMEWORK_PACKAGE_ID};
use tracing::info;

use sui_tx::sui_client::{Network, Signer, SuiClientWrapper};
use sui_tx::tx::shared_object_arg;

// ---- DeepBook v3 testnet addresses (from the @mysten/deepbook-v3 SDK) -------
const DEEPBOOK_PKG: &str = "0x22be4cade64bf2d02412c7e8d0e8beea2f78828b948118d46735315409371a3c";
const REGISTRY_ID: &str = "0x7c256edbda983a2cd6f946655f4bf3f00a41043993781f8674a7046e8c0e11d1";
/// Whitelisted DEEP/SUI pool (base = DEEP, quote = SUI). Whitelisted => no DEEP
/// fee on swaps, so we can swap paying a zero DEEP coin.
const DEEP_SUI_POOL: &str = "0x48c95963e9eac37a316b7ae04a0deb761bcdcc2b67912374d6036e7f0e9bae9f";
const DEEP_TYPE: &str =
    "0x36dbef866a1d62bf7328989a10fb2f07d769f4ee587c0de4a0a256e57e0a58a8::deep::DEEP";
const SUI_TYPE: &str = "0x2::sui::SUI";

// ---- Our test tokens (testnet, deployments.json `prod`) ---------------------
const TBTC_TYPE: &str =
    "0x159cc8d63af67b12286a9a7868a40b274ef352960b1a3d9ea31c0169cc3efa73::tbtc::TBTC";
const TUSDC_TYPE: &str =
    "0x159cc8d63af67b12286a9a7868a40b274ef352960b1a3d9ea31c0169cc3efa73::tusdc::TUSDC";

/// DeepBook pool-creation fee: 500 DEEP, DEEP has 6 decimals. Verified live via
/// `constants::pool_creation_fee()` dev-inspect.
const POOL_CREATION_FEE: u64 = 500_000_000;

// ---- Pool params for TBTC(base, 8 dec) / TUSDC(quote, 6 dec) ----------------
// lot_size & min_size must be powers of 10 with 1000 <= lot <= min.
const TICK_SIZE: u64 = 10_000;
const LOT_SIZE: u64 = 1_000;
const MIN_SIZE: u64 = 10_000;

const SWAP_GAS_BUDGET: u64 = 100_000_000; // 0.1 SUI
const CREATE_GAS_BUDGET: u64 = 200_000_000; // 0.2 SUI
/// Leave this much SUI untouched for gas when sizing a swap.
const GAS_RESERVE_MIST: u128 = 300_000_000;

#[derive(Parser)]
#[command(about = "Create + verify a DeepBook v3 permissionless TBTC/TUSDC pool on testnet")]
struct Cli {
    /// Path to the secrets TOML holding `[sui] testnet = "suiprivkey1..."`.
    #[arg(long, default_value = "tools/deepbook-pool-test/config/secrets.toml")]
    secrets: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print address, balances, fee, and readiness.
    Status,
    /// Best-effort acquire DEEP by swapping SUI through the DEEP/SUI pool.
    AcquireDeep,
    /// Create Pool<TBTC, TUSDC> (requires >= 500 DEEP).
    Create,
    /// Read back a pool object and print its parsed config.
    Verify {
        /// Pool object id.
        pool_id: String,
    },
    /// status -> create (if ready) -> verify.
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let wrap = SuiClientWrapper::connect(&secrets, Network::Testnet).await?;
    let client = &wrap.client;
    let signer = &wrap.signer;

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Status => {
            status(client, signer.address).await?;
        }
        Cmd::AcquireDeep => {
            acquire_deep(client, signer, POOL_CREATION_FEE as u128).await?;
            status(client, signer.address).await?;
        }
        Cmd::Create => {
            create_and_verify(client, signer).await?;
        }
        Cmd::Verify { pool_id } => {
            verify_pool(client, oid(&pool_id)?).await?;
        }
        Cmd::Run => {
            let ready = status(client, signer.address).await?;
            if !ready {
                println!(
                    "\nNot ready: wallet needs >= {} DEEP. Acquire DEEP into {} \
                     (no testnet faucet — Discord/another holder/FlowX), then re-run.\n\
                     (`acquire-deep` will try the DEEP/SUI pool but testnet depth is ~20 DEEP.)",
                    fmt(POOL_CREATION_FEE as u128, 6),
                    signer.address
                );
                return Ok(());
            }
            create_and_verify(client, signer).await?;
        }
    }
    Ok(())
}

/// Print readiness; returns true if the wallet can pay the creation fee.
async fn status(client: &ChainClient, addr: SuiAddress) -> Result<bool> {
    let sui = balance(client, addr, SUI_TYPE).await?;
    let deep = balance(client, addr, DEEP_TYPE).await?;
    let tbtc = balance(client, addr, TBTC_TYPE).await?;
    let tusdc = balance(client, addr, TUSDC_TYPE).await?;
    let ready = deep >= POOL_CREATION_FEE as u128;

    println!("network : testnet");
    println!("address : {addr}");
    println!("balances:");
    println!("  SUI  : {}", fmt(sui, 9));
    println!("  DEEP : {}", fmt(deep, 6));
    println!("  TBTC : {}", fmt(tbtc, 8));
    println!("  TUSDC: {}", fmt(tusdc, 6));
    println!(
        "pool    : TBTC/TUSDC  (tick={TICK_SIZE} lot={LOT_SIZE} min={MIN_SIZE})"
    );
    println!(
        "fee     : {} DEEP required  ->  {}",
        fmt(POOL_CREATION_FEE as u128, 6),
        if ready { "READY ✅" } else { "NOT READY ❌ (need more DEEP)" }
    );
    Ok(ready)
}

/// Total balance of `coin_type` held by `addr`, in smallest units.
async fn balance(client: &ChainClient, addr: SuiAddress, coin_type: &str) -> Result<u128> {
    let tag = sui_types::parse_sui_struct_tag(coin_type)
        .map_err(|e| anyhow!("parsing coin type {coin_type}: {e}"))?;
    client
        .balance(addr, &tag)
        .await
        .with_context(|| format!("reading {coin_type} balance"))
}

/// Create the pool, then read it back to confirm.
async fn create_and_verify(client: &ChainClient, signer: &Signer) -> Result<()> {
    let deep = balance(client, signer.address, DEEP_TYPE).await?;
    if deep < POOL_CREATION_FEE as u128 {
        bail!(
            "wallet holds {} DEEP but needs {}. Fund {} with DEEP and retry.",
            fmt(deep, 6),
            fmt(POOL_CREATION_FEE as u128, 6),
            signer.address
        );
    }

    println!("creating Pool<TBTC, TUSDC> (tick={TICK_SIZE} lot={LOT_SIZE} min={MIN_SIZE})...");
    let resp = create_pool(client, signer).await?;
    let digest = sui_tx::tx::tx_digest(&resp);
    let pool_id = created_objects(&resp)
        .into_iter()
        .find_map(|c| pool_id_of(&c))
        .ok_or_else(|| anyhow!("tx succeeded but no Pool object found in object changes"))?;

    println!("\n✅ pool created");
    println!("  pool id : {pool_id}");
    println!("  tx      : {digest}");
    println!("  explorer: https://suiscan.xyz/testnet/object/{pool_id}");
    println!("\nverifying on-chain...");
    verify_pool(client, pool_id).await?;
    Ok(())
}

/// Loop: swap testnet SUI for DEEP until we hold `target` DEEP units.
/// Rate-adaptive — probes with 1 SUI, then sizes the next swap from the
/// observed DEEP-per-MIST rate. Best-effort: testnet DEEP/SUI depth is thin.
async fn acquire_deep(client: &ChainClient, signer: &Signer, target: u128) -> Result<()> {
    let addr = signer.address;
    let mut rate: Option<f64> = None; // DEEP units per MIST of SUI

    for iter in 0..8u32 {
        let have = balance(client, addr, DEEP_TYPE).await?;
        if have >= target {
            return Ok(());
        }
        let needed = target - have;

        let want_sui = match rate {
            Some(r) if r > 0.0 => ((needed as f64 / r) * 1.3).ceil() as u128,
            _ => 1_000_000_000, // probe: 1 SUI
        };
        let sui_bal = balance(client, addr, SUI_TYPE).await?;
        let spendable = sui_bal.saturating_sub(GAS_RESERVE_MIST);
        let sui_in = want_sui.min(spendable);
        if sui_in == 0 {
            bail!(
                "need ~{} more DEEP but only {} SUI spendable (after gas reserve). \
                 Run `sui client faucet` for {} and retry.",
                fmt(needed, 6),
                fmt(sui_bal, 9),
                addr
            );
        }
        let sui_in = u64::try_from(sui_in).context("sui_in overflow")?;

        info!(iter, sui_in, "swapping SUI -> DEEP");
        let before = have;
        swap_sui_for_deep(client, signer, sui_in).await?;
        let after = balance(client, addr, DEEP_TYPE).await?;
        let got = after.saturating_sub(before);
        if got == 0 {
            bail!(
                "swap yielded 0 DEEP — the DEEP/SUI pool has no ask-side liquidity. \
                 Testnet DeepBook books hold only ~20 DEEP and there is no public DEEP \
                 faucet, so the 500 DEEP fee cannot be self-funded from on-chain \
                 liquidity. Fund {} with >= 500 DEEP by other means, then re-run.",
                addr
            );
        }
        rate = Some(got as f64 / sui_in as f64);
        info!(got_deep = %fmt(got, 6), spent_sui = %fmt(sui_in as u128, 9), "swap done");
    }
    bail!("could not accumulate {} DEEP within 8 swaps", fmt(target, 6))
}

/// One SUI->DEEP swap through the whitelisted DEEP/SUI pool.
/// `swap_exact_quote_for_base<DEEP, SUI>(pool, sui_in, zero_deep, 0, clock)`.
async fn swap_sui_for_deep(client: &ChainClient, signer: &Signer, sui_in: u64) -> Result<()> {
    let mut pt = ProgrammableTransactionBuilder::new();

    let pool = pt.obj(shared_object_arg(client, oid(DEEP_SUI_POOL)?, true).await?)?;
    let clock = pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?;

    let deep_tag = TypeTag::from_str(DEEP_TYPE)?;
    let sui_tag = TypeTag::from_str(SUI_TYPE)?;

    // Split the SUI we want to spend off the gas coin.
    let amt = pt.pure(sui_in)?;
    let split = pt.command(Command::SplitCoins(Argument::GasCoin, vec![amt]));
    let sui_coin = nested(split, 0)?;

    // Zero DEEP fee coin (allowed on whitelisted pools).
    let deep_zero = pt.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("coin").unwrap(),
        Identifier::new("zero").unwrap(),
        vec![deep_tag.clone()],
        vec![],
    );

    let min_out = pt.pure(0u64)?;
    let swap = pt.programmable_move_call(
        oid(DEEPBOOK_PKG)?,
        Identifier::new("pool").unwrap(),
        Identifier::new("swap_exact_quote_for_base").unwrap(),
        vec![deep_tag, sui_tag], // <BaseAsset = DEEP, QuoteAsset = SUI>
        vec![pool, sui_coin, deep_zero, min_out, clock],
    );

    // Returns (Coin<DEEP>, Coin<SUI>, Coin<DEEP>) — send them all back to us.
    let base_out = nested(swap, 0)?;
    let quote_left = nested(swap, 1)?;
    let deep_left = nested(swap, 2)?;
    let recipient = pt.pure(signer.address)?;
    pt.command(Command::TransferObjects(
        vec![base_out, quote_left, deep_left],
        recipient,
    ));

    sign_submit(client, signer, pt, SWAP_GAS_BUDGET).await?;
    Ok(())
}

/// Build + submit the create_permissionless_pool<TBTC, TUSDC> PTB.
async fn create_pool(client: &ChainClient, signer: &Signer) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();

    let registry = pt.obj(shared_object_arg(client, oid(REGISTRY_ID)?, true).await?)?;

    // Gather DEEP coins as owned inputs; merge into one, split out exactly 500.
    let deep_tag = sui_types::parse_sui_struct_tag(DEEP_TYPE)
        .map_err(|e| anyhow!("parsing DEEP type: {e}"))?;
    let deep_coins = client
        .coins(signer.address, &deep_tag)
        .await
        .context("listing DEEP coins")?;
    if deep_coins.is_empty() {
        bail!("no DEEP coins owned");
    }
    let mut deep_args = Vec::with_capacity(deep_coins.len());
    for c in &deep_coins {
        deep_args.push(pt.obj(ObjectArg::ImmOrOwnedObject(c.object_ref))?);
    }
    let primary = deep_args[0];
    if deep_args.len() > 1 {
        pt.command(Command::MergeCoins(primary, deep_args[1..].to_vec()));
    }
    let fee_amt = pt.pure(POOL_CREATION_FEE)?;
    let split = pt.command(Command::SplitCoins(primary, vec![fee_amt]));
    let fee_coin = nested(split, 0)?;

    let tick = pt.pure(TICK_SIZE)?;
    let lot = pt.pure(LOT_SIZE)?;
    let min = pt.pure(MIN_SIZE)?;

    let base_tag = TypeTag::from_str(TBTC_TYPE)?;
    let quote_tag = TypeTag::from_str(TUSDC_TYPE)?;

    // create_permissionless_pool returns `ID` (has drop) — fine to ignore.
    pt.programmable_move_call(
        oid(DEEPBOOK_PKG)?,
        Identifier::new("pool").unwrap(),
        Identifier::new("create_permissionless_pool").unwrap(),
        vec![base_tag, quote_tag],
        vec![registry, tick, lot, min, fee_coin],
    );

    sign_submit(client, signer, pt, CREATE_GAS_BUDGET).await
}

/// Read back a pool object and print its type + key fields.
async fn verify_pool(client: &ChainClient, pool_id: ObjectID) -> Result<()> {
    let object = client
        .get_object(pool_id)
        .await
        .context("reading pool object")?;
    let ty = object
        .struct_tag()
        .map(|t| t.to_canonical_string(/* with_prefix */ true))
        .unwrap_or_default();
    if !ty.contains("::pool::Pool<") {
        bail!("object {pool_id} is not a DeepBook Pool (type: {ty})");
    }
    println!("  ✅ verified: {pool_id}");
    println!("     type: {ty}");
    Ok(())
}

/// Gas-select (all SUI coins), dry-run, then sign + submit. Bails on revert.
async fn sign_submit(
    client: &ChainClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let programmable = pt.finish();

    let sui_coins = client
        .coins(signer.address, &sui_tx::chain::sui_coin_type())
        .await
        .context("listing gas coins")?;
    if sui_coins.is_empty() {
        bail!("no SUI coins to pay gas for {}", signer.address);
    }
    let gas_refs: Vec<_> = sui_coins.iter().map(|c| c.object_ref).collect();
    let gas_price = client.reference_gas_price().await?;

    let tx_data = TransactionData::new_programmable(
        signer.address,
        gas_refs,
        programmable,
        gas_budget,
        gas_price,
    );

    // Dry-run first so bad assumptions fail without spending.
    let dry = client.dry_run(&tx_data).await.context("dry-run")?;
    {
        use sui_types::effects::TransactionEffectsAPI;
        let status = dry.transaction.effects.status();
        if status.is_err() {
            bail!("dry-run reverted: {status:?}");
        }
    }

    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let resp = client.execute(&tx).await.context("submitting tx")?;
    sui_tx::tx::assert_success(&resp, "tx")?;
    Ok(resp)
}

/// Pull the created DeepBook `Pool<...>` object id out of an object change.
fn pool_id_of(c: &sui_tx::chain::ChangedObject) -> Option<ObjectID> {
    c.object_type
        .contains("::pool::Pool<")
        .then_some(c.object_id)
}

/// `Argument::Result(i)` -> `Argument::NestedResult(i, j)`, for indexing
/// multi-value command results (SplitCoins, the swap's 3-coin return).
fn nested(arg: Argument, j: u16) -> Result<Argument> {
    match arg {
        Argument::Result(i) => Ok(Argument::NestedResult(i, j)),
        other => Err(anyhow!("expected Result argument, got {other:?}")),
    }
}

fn oid(s: &str) -> Result<ObjectID> {
    ObjectID::from_str(s).with_context(|| format!("parsing object id {s}"))
}

/// Format a smallest-units amount with `decimals` for display.
fn fmt(units: u128, decimals: u32) -> String {
    let scale = 10u128.pow(decimals);
    format!(
        "{}.{:0width$}",
        units / scale,
        units % scale,
        width = decimals as usize
    )
}
