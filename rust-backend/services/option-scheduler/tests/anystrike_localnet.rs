//! Any-strike E2E against a live Sui localnet: the publish-free bucket path.
//!
//! Proves on a real validator what `contracts/core/tests/any_strike_tests.move`
//! proves in-unit — plus the parts unit tests cannot reach:
//!
//!  1. the `private_generics` verifier accepts `option_coin`'s runtime
//!     `coin_registry::new_currency<OptionCall<…>>` call at publish time;
//!  2. ONE atomic PTB performs create-bucket (arbitrary strike) → covered
//!     write against the not-yet-shared bucket → share, with the option
//!     coin minted to the sender in the same transaction;
//!  3. a second create at the same normalized spec aborts.
//!
//! Also covers the scheduler's publish-free grid roll (`roller::submit`),
//! which replaced the retired codegen→publish→harvest pipeline (and the old
//! `localnet_e2e.rs` harness with it).
//!
//! `#[ignore]` — needs a running localnet with the coin registry object
//! (`0xc`), i.e. any current `sui start --force-regenesis --with-faucet`.
//! Run with:
//!   cargo test -p option-scheduler --test anystrike_localnet -- --ignored --nocapture
//! RPC/faucet override: SUI_E2E_RPC / SUI_E2E_FAUCET.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::{StructTag, TypeTag};
use sui_tx::chain::{created_objects, published_package, ChainClient};
use sui_tx::sui_client::Signer;
use sui_tx::tx::{owned_object_arg, shared_object_arg, submit_ptb};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::{get_key_pair, AccountKeyPair, SuiKeyPair};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command};

const GAS: u64 = 500_000_000;
const CLOCK_ID: &str = "0x6";
const COIN_REGISTRY_ID: &str = "0xc";

// 100_000_000 minutes → expiry_ms = 6e12 (minute-aligned by construction).
const EXPIRY_MINUTES: u32 = 100_000_000;
const EXPIRY_MS: u64 = EXPIRY_MINUTES as u64 * 60_000;
// strike 50_000 at scale 0 is already normalized (exp floors at zero).
const SIG: u64 = 50_000;
const EXP: u8 = 0;
const WRITE_AMOUNT: u64 = 1_000_000;

/// Stage a pristine copy of core + whitelist (sources and Move.toml only —
/// no Move.lock / Published.toml). The checked-in publish records pin
/// testnet addresses, which makes the builder treat the packages as already
/// published (0 root modules) and would break a localnet publish anyway.
fn stage_contracts() -> PathBuf {
    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../contracts")
        .canonicalize()
        .expect("contracts dir");
    // Unique per call: the two localnet tests run in parallel and must not
    // clobber each other's staging copy mid-compile.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stage = std::env::temp_dir().join(format!(
        "anystrike-e2e-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&stage);
    for pkg in ["core", "whitelist"] {
        let dst = stage.join(pkg);
        std::fs::create_dir_all(dst.join("sources")).expect("mkdir");
        std::fs::copy(src_root.join(pkg).join("Move.toml"), dst.join("Move.toml"))
            .expect("copy Move.toml");
        for entry in std::fs::read_dir(src_root.join(pkg).join("sources")).expect("sources") {
            let entry = entry.expect("entry");
            if entry.path().extension().is_some_and(|e| e == "move") {
                std::fs::copy(entry.path(), dst.join("sources").join(entry.file_name()))
                    .expect("copy module");
            }
        }
    }
    stage.join("core")
}

/// The 10 byte-marker type args spelling (expiry_minutes, sig, exp) —
/// mirrors `option_coin`'s layout: minutes u32 (4 bytes) ‖ significand u40
/// (5 bytes) ‖ exponent u8, MSB first. Markers `B00..B7F` live in `enc0`,
/// `B80..BFF` in `enc1` (Sui caps datatype definitions per module). Ten
/// flat markers keep every option entry point within the 15-type-node
/// PTB budget (`exercise<U, S, OptionCall<U, S, D0..D9>>` = exactly 15).
fn marker_type_args(package: ObjectID, expiry_minutes: u32, sig: u64, exp: u8) -> Vec<TypeTag> {
    assert!(sig <= 0xFF_FFFF_FFFF, "sig exceeds u40");
    let mut bytes = expiry_minutes.to_be_bytes().to_vec();
    bytes.extend_from_slice(&sig.to_be_bytes()[3..]); // low 5 of 8
    bytes.push(exp);
    bytes
        .into_iter()
        .map(|b| {
            let module = if b < 0x80 { "enc0" } else { "enc1" };
            TypeTag::from_str(&format!("{package}::{module}::B{b:02X}"))
                .expect("marker type tag")
        })
        .collect()
}

/// `OptionCall<U, S, …markers…>` as a TypeTag.
fn option_call_tag(package: ObjectID, u: &TypeTag, s: &TypeTag) -> TypeTag {
    let mut params = vec![u.clone(), s.clone()];
    params.extend(marker_type_args(package, EXPIRY_MINUTES, SIG, EXP));
    TypeTag::Struct(Box::new(StructTag {
        address: package.into(),
        module: Identifier::new("option_coin").unwrap(),
        name: Identifier::new("OptionCall").unwrap(),
        type_params: params,
    }))
}

async fn fund(faucet: &str, addr: SuiAddress) -> Result<()> {
    let body = serde_json::json!({ "FixedAmountRequest": { "recipient": addr.to_string() } });
    let resp = reqwest::Client::new()
        .post(format!("{faucet}/gas"))
        .json(&body)
        .send()
        .await
        .context("faucet request")?;
    if !resp.status().is_success() {
        return Err(anyhow!("faucet returned {}", resp.status()));
    }
    Ok(())
}

async fn wait_for_gas(client: &ChainClient, addr: SuiAddress) -> Result<()> {
    for _ in 0..30 {
        let coins = client.coins(addr, &sui_tx::chain::sui_coin_type()).await?;
        if !coins.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("address {addr} never received gas from faucet"))
}

struct Deployed {
    package: ObjectID,
    bucket_registry: ObjectID,
    whitelist: ObjectID,
    wl_admin_cap: ObjectID,
}

/// Compile + publish `contracts/`, harvesting the shared registries and the
/// whitelist admin cap from the publish effects.
async fn publish_protocol(
    client: &ChainClient,
    signer: &Signer,
    dir: &std::path::Path,
) -> Result<Deployed> {
    // Compile via the sui CLI: the vendored `sui-move-build` drops bundled
    // unpublished-dependency modules from `get_package_bytes(true)` (12 vs
    // the CLI's 13 here), which lands as PublishUpgradeMissingDependency.
    // The CLI's env-aware build handles the bundling correctly.
    let out = std::process::Command::new("sui")
        .args([
            "move", "build", "--dump-bytecode-as-base64",
            "--with-unpublished-dependencies", "--build-env", "testnet",
        ])
        .current_dir(dir)
        .output()
        .context("running sui move build")?;
    if !out.status.success() {
        return Err(anyhow!(
            "sui move build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    #[derive(serde::Deserialize)]
    struct Dump {
        modules: Vec<String>,
        dependencies: Vec<String>,
    }
    let dump: Dump = serde_json::from_slice(&out.stdout).context("parsing bytecode dump")?;
    use base64::Engine as _;
    let modules: Vec<Vec<u8>> = dump
        .modules
        .iter()
        .map(|m| base64::engine::general_purpose::STANDARD.decode(m).context("module b64"))
        .collect::<Result<_>>()?;
    let deps: Vec<ObjectID> = dump
        .dependencies
        .iter()
        .map(|d| ObjectID::from_str(d).context("dep id"))
        .collect::<Result<_>>()?;
    eprintln!("publish: {} modules, deps = {:?}", modules.len(), deps);

    let mut pt = ProgrammableTransactionBuilder::new();
    let cap = pt.publish_upgradeable(modules, deps);
    pt.transfer_arg(signer.address, cap);
    let resp = submit_ptb(client, signer, pt, GAS, "protocol publish").await?;

    let mut bucket_registry = None;
    let mut whitelist = None;
    let mut wl_admin_cap = None;
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        match (tag.module.as_str(), tag.name.as_str()) {
            ("bucket_registry", "BucketRegistry") => bucket_registry = Some(c.object_id),
            ("whitelist", "Whitelist") => whitelist = Some(c.object_id),
            ("whitelist", "AdminCap") => wl_admin_cap = Some(c.object_id),
            _ => {}
        }
    }
    Ok(Deployed {
        package: published_package(&resp).ok_or_else(|| anyhow!("no package id"))?,
        bucket_registry: bucket_registry.ok_or_else(|| anyhow!("no BucketRegistry"))?,
        whitelist: whitelist.ok_or_else(|| anyhow!("no Whitelist"))?,
        wl_admin_cap: wl_admin_cap.ok_or_else(|| anyhow!("no whitelist AdminCap"))?,
    })
}

/// The atomic any-strike PTB:
///   create_bucket_any_strike → write_collateralized(&mut bucket) →
///   TransferObjects(position, call) → share_bucket(bucket)
async fn build_create_write_ptb(
    client: &ChainClient,
    signer: &Signer,
    d: &Deployed,
) -> Result<ProgrammableTransactionBuilder> {
    let sui: TypeTag = TypeTag::from_str("0x2::sui::SUI")?;
    let call_tag = option_call_tag(d.package, &sui, &sui);
    let mut create_targs = vec![sui.clone(), sui.clone()];
    create_targs.extend(marker_type_args(d.package, EXPIRY_MINUTES, SIG, EXP));

    let mut pt = ProgrammableTransactionBuilder::new();
    let breg = pt.obj(shared_object_arg(client, d.bucket_registry, true).await?)?;
    let creg =
        pt.obj(shared_object_arg(client, ObjectID::from_str(COIN_REGISTRY_ID)?, true).await?)?;
    let wl = pt.obj(shared_object_arg(client, d.whitelist, false).await?)?;
    let clock = pt.obj(shared_object_arg(client, ObjectID::from_str(CLOCK_ID)?, false).await?)?;
    let expiry = pt.pure(&EXPIRY_MS)?;
    let strike = pt.pure(&(SIG as u128))?;
    let scale = pt.pure(&EXP)?;
    let decimals = pt.pure(&9u8)?;

    // Command 0: create — the bucket value is Result(0).
    let bucket = pt.programmable_move_call(
        d.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("create_bucket_any_strike").unwrap(),
        create_targs,
        vec![breg, creg, wl, expiry, strike, scale, decimals, clock],
    );

    // Command 1: carve the write collateral off gas.
    let amount = pt.pure(&WRITE_AMOUNT)?;
    let underlying = pt.command(Command::SplitCoins(Argument::GasCoin, vec![amount]));

    // Command 2: covered write against the not-yet-shared bucket.
    let out = pt.programmable_move_call(
        d.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("write_collateralized").unwrap(),
        vec![sui.clone(), sui.clone(), call_tag.clone()],
        vec![bucket, wl, underlying, clock],
    );

    // Command 3: position + option coin to the writer.
    let position = Argument::NestedResult(2, 0);
    let call_coin = Argument::NestedResult(2, 1);
    let _ = out;
    let recipient = pt.pure(&signer.address)?;
    pt.command(Command::TransferObjects(vec![position, call_coin], recipient));

    // Command 4: terminal share.
    pt.programmable_move_call(
        d.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("share_bucket").unwrap(),
        vec![sui.clone(), sui, call_tag],
        vec![bucket],
    );
    Ok(pt)
}

#[tokio::test]
#[ignore = "requires a running sui localnet (sui start --force-regenesis --with-faucet)"]
async fn localnet_any_strike_atomic_create_write() -> Result<()> {
    let rpc = std::env::var("SUI_E2E_RPC").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let faucet =
        std::env::var("SUI_E2E_FAUCET").unwrap_or_else(|_| "http://127.0.0.1:9123".into());

    let (address, kp): (SuiAddress, AccountKeyPair) = get_key_pair();
    let signer = Signer { keypair: SuiKeyPair::Ed25519(kp), address };
    let client = ChainClient::new(&rpc)?;
    for _ in 0..3 {
        fund(&faucet, address).await?;
    }
    wait_for_gas(&client, address).await?;

    // Publish — this alone proves the verifier accepts option_coin's
    // runtime new_currency call over a generic root.
    let d = publish_protocol(&client, &signer, &stage_contracts()).await?;
    eprintln!("protocol package = {}", d.package);
    // Fresh objects can lag the fullnode's read path by a beat.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Whitelist the writer (ingress gate covers creation + writes).
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap = pt.obj(owned_object_arg(&client, d.wl_admin_cap).await?)?;
    let wl = pt.obj(shared_object_arg(&client, d.whitelist, true).await?)?;
    let member = pt.pure(&address)?;
    pt.programmable_move_call(
        d.package,
        Identifier::new("whitelist").unwrap(),
        Identifier::new("add_member").unwrap(),
        vec![],
        vec![cap, wl, member],
    );
    submit_ptb(&client, &signer, pt, GAS, "whitelist writer").await?;

    // THE atomic transaction.
    let pt = build_create_write_ptb(&client, &signer, &d).await?;
    let resp = submit_ptb(&client, &signer, pt, GAS, "any-strike create+write").await?;

    // A shared Bucket<SUI, SUI, OptionCall<…>> must exist, plus the writer's
    // Position and Coin<OptionCall<…>> and the registry's Currency object.
    let sui: TypeTag = TypeTag::from_str("0x2::sui::SUI")?;
    let call_tag = option_call_tag(d.package, &sui, &sui);
    let call_str = call_tag.to_canonical_string(true);
    let mut bucket_id = None;
    let mut saw_position = false;
    let mut saw_coin = false;
    let mut saw_currency = false;
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        match (tag.module.as_str(), tag.name.as_str()) {
            ("bucket", "Bucket") => {
                assert_eq!(tag.type_params.len(), 3);
                assert_eq!(tag.type_params[2].to_canonical_string(true), call_str);
                bucket_id = Some(c.object_id);
            }
            ("position", "Position") => saw_position = true,
            ("coin", "Coin") => {
                if tag.type_params[0].to_canonical_string(true) == call_str {
                    saw_coin = true;
                }
            }
            ("coin_registry", "Currency") => saw_currency = true,
            _ => {}
        }
    }
    let bucket_id = bucket_id.ok_or_else(|| anyhow!("no Bucket created"))?;
    assert!(saw_position, "writer Position not created");
    assert!(saw_coin, "option Coin<OptionCall<…>> not minted to writer");
    assert!(saw_currency, "coin_registry Currency<OptionCall<…>> not created");
    eprintln!("✓ atomic create+write landed; bucket = {bucket_id}");

    // Duplicate spec (different raw form, same normalized economics) must fail.
    let mut pt = ProgrammableTransactionBuilder::new();
    let breg = pt.obj(shared_object_arg(&client, d.bucket_registry, true).await?)?;
    let creg =
        pt.obj(shared_object_arg(&client, ObjectID::from_str(COIN_REGISTRY_ID)?, true).await?)?;
    let wl = pt.obj(shared_object_arg(&client, d.whitelist, false).await?)?;
    let clock = pt.obj(shared_object_arg(&client, ObjectID::from_str(CLOCK_ID)?, false).await?)?;
    let expiry = pt.pure(&EXPIRY_MS)?;
    let strike = pt.pure(&((SIG as u128) * 100))?; // 5_000_000 at scale 2 → (50_000, 0)
    let scale = pt.pure(&2u8)?;
    let decimals = pt.pure(&9u8)?;
    let mut create_targs = vec![sui.clone(), sui.clone()];
    create_targs.extend(marker_type_args(d.package, EXPIRY_MINUTES, SIG, EXP));
    let bucket = pt.programmable_move_call(
        d.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("create_bucket_any_strike").unwrap(),
        create_targs,
        vec![breg, creg, wl, expiry, strike, scale, decimals, clock],
    );
    let _ = bucket;
    pt.programmable_move_call(
        d.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("share_bucket").unwrap(),
        vec![sui.clone(), sui, call_tag],
        vec![bucket],
    );
    let dup = submit_ptb(&client, &signer, pt, GAS, "duplicate create (must fail)").await;
    assert!(dup.is_err(), "duplicate normalized spec unexpectedly succeeded");
    eprintln!("✓ duplicate spec rejected");
    Ok(())
}


/// The scheduler's grid roll: one publish-free PTB, N buckets + N distinct
/// runtime currencies. Mirrors production `tick_once` → `roller::submit`.
#[tokio::test]
#[ignore = "requires a running sui localnet (sui start --force-regenesis --with-faucet)"]
async fn localnet_roller_grid_roll() -> Result<()> {
    use option_scheduler::roller::{self, ProductType, RollPlan};
    use option_scheduler::strike_grid::StrikeGrid;
    use sui_tx::sui_client::{Network, SuiClientWrapper};

    let rpc = std::env::var("SUI_E2E_RPC").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let faucet =
        std::env::var("SUI_E2E_FAUCET").unwrap_or_else(|_| "http://127.0.0.1:9123".into());

    let (address, kp): (SuiAddress, AccountKeyPair) = get_key_pair();
    let signer = Signer { keypair: SuiKeyPair::Ed25519(kp), address };
    let client = ChainClient::new(&rpc)?;
    for _ in 0..3 {
        fund(&faucet, address).await?;
    }
    wait_for_gas(&client, address).await?;

    let d = publish_protocol(&client, &signer, &stage_contracts()).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Whitelist the roller (creation is ingress-gated).
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap = pt.obj(owned_object_arg(&client, d.wl_admin_cap).await?)?;
    let wl = pt.obj(shared_object_arg(&client, d.whitelist, true).await?)?;
    let member = pt.pure(&address)?;
    pt.programmable_move_call(
        d.package,
        Identifier::new("whitelist").unwrap(),
        Identifier::new("add_member").unwrap(),
        vec![],
        vec![cap, wl, member],
    );
    submit_ptb(&client, &signer, pt, GAS, "whitelist roller").await?;

    let plan = RollPlan {
        underlying_symbol: "SUI".into(),
        settlement_symbol: "SUI".into(),
        underlying_type: "0x2::sui::SUI".into(),
        settlement_type: "0x2::sui::SUI".into(),
        underlying_decimals: 9,
        settlement_decimals: 9,
        expiry_ms: EXPIRY_MS + 60_000, // distinct family from the other test
        strikes: StrikeGrid {
            start_strike: 50_000,
            strike_interval: 1_000,
            count: 2,
            strike_scale: 0,
        }
        .strikes(),
        strike_scale: 0,
        product_type: ProductType::Call,
    };
    let wrap = SuiClientWrapper {
        client,
        events: sui_tx::events::EventClient::new(Network::Devnet.graphql_url()),
        signer,
        network: Network::Devnet,
    };
    let roll_ctx = sui_tx::tx::coin_pkg::AnyStrikeContext {
        package: d.package,
        bucket_registry: d.bucket_registry,
        whitelist: d.whitelist,
    };
    let out = roller::submit(&wrap, &roll_ctx, &plan, None, None, GAS).await?;
    assert_eq!(out.bucket_ids.len(), 2, "expected two buckets");
    // Fresh objects can lag the fullnode's read path by a beat.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut call_types = Vec::new();
    for bid in &out.bucket_ids {
        let obj = wrap.client.get_object(*bid).await?;
        let tag = obj
            .struct_tag()
            .ok_or_else(|| anyhow!("bucket {bid} has no struct type"))?;
        assert_eq!(tag.type_params.len(), 3, "Bucket should have 3 type params");
        let call = tag.type_params[2].to_canonical_string(true);
        assert!(
            call.contains("::option_coin::OptionCall<"),
            "3rd type param should be a runtime option-coin currency, got {call}"
        );
        call_types.push(call);
    }
    call_types.sort();
    call_types.dedup();
    assert_eq!(call_types.len(), 2, "each bucket must have a distinct coin type");
    eprintln!("✓ publish-free grid roll created 2 buckets: {call_types:?}");
    Ok(())
}
