//! End-to-end roll against a live Sui localnet.
//!
//! Verifies the whole per-bucket-coin pipeline on a real validator: publish
//! the protocol package, then run a real roll (codegen → in-process compile →
//! publish the option-coin package → harvest TreasuryCaps → `create_bucket`
//! per strike), then read the buckets back and assert each is a distinct
//! `Bucket<U, S, Call_i>` with its own zero-supply option coin.
//!
//! `#[ignore]` — needs a running localnet. Bring one up with:
//!   sui start --force-regenesis --with-faucet
//! then run:
//!   cargo test -p option-scheduler --test localnet_e2e -- --ignored --nocapture
//!
//! RPC/faucet default to localnet ports; override with SUI_E2E_RPC /
//! SUI_E2E_FAUCET.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    ObjectChange, SuiObjectDataOptions, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_move_build::BuildConfig;
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::{ObjectID, ObjectType, SuiAddress};
use sui_types::crypto::{get_key_pair, AccountKeyPair, SuiKeyPair};
use sui_types::transaction::Transaction;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;

use option_scheduler::roller::{self, ProductType, RollPlan};
use option_scheduler::strike_grid::StrikeGrid;
use sui_tx::sui_client::{Network, Signer, SuiClientWrapper};

const GAS: u64 = 500_000_000;

fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../contracts")
        .canonicalize()
        .expect("contracts dir")
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

async fn wait_for_gas(client: &SuiClient, addr: SuiAddress) -> Result<()> {
    for _ in 0..30 {
        let coins = client
            .coin_read_api()
            .get_coins(addr, None, None, Some(1))
            .await?;
        if !coins.data.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("address {addr} never received gas from faucet"))
}

/// Compile + publish `contracts/`, returning (package_id, admin_cap_id).
async fn publish_protocol(
    client: &SuiClient,
    signer: &Signer,
    dir: &std::path::Path,
) -> Result<(ObjectID, ObjectID)> {
    let compiled = BuildConfig::new_for_testing()
        .build(dir)
        .context("compiling protocol package")?;
    let modules = compiled.get_package_bytes(false);
    let deps = compiled.get_dependency_storage_package_ids();

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, GAS)
        .await
        .context("building protocol publish tx")?;
    let sig =
        Transaction::signature_from_signer(tx_data.clone(), Intent::sui_transaction(), &signer.keypair);
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting protocol publish")?;
    assert_ok(&resp)?;

    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("publish missing object_changes"))?;
    let mut package = None;
    let mut admin_cap = None;
    for c in changes {
        match c {
            ObjectChange::Published { package_id, .. } => package = Some(*package_id),
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "admin"
                && object_type.name.as_str() == "AdminCap" =>
            {
                admin_cap = Some(*object_id)
            }
            _ => {}
        }
    }
    Ok((
        package.ok_or_else(|| anyhow!("no package id"))?,
        admin_cap.ok_or_else(|| anyhow!("no admin cap"))?,
    ))
}

fn assert_ok(resp: &SuiTransactionBlockResponse) -> Result<()> {
    use sui_json_rpc_types::SuiTransactionBlockEffectsAPI;
    let e = resp.effects.as_ref().ok_or_else(|| anyhow!("no effects"))?;
    if e.status().is_err() {
        return Err(anyhow!("tx failed: {:?}", e.status()));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running sui localnet (sui start --force-regenesis --with-faucet)"]
async fn localnet_roll_creates_per_bucket_coins() -> Result<()> {
    let rpc = std::env::var("SUI_E2E_RPC").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let faucet =
        std::env::var("SUI_E2E_FAUCET").unwrap_or_else(|_| "http://127.0.0.1:9123".into());

    // Fresh funded key.
    let (address, kp): (SuiAddress, AccountKeyPair) = get_key_pair();
    let signer = Signer {
        keypair: SuiKeyPair::Ed25519(kp),
        address,
    };
    let client = SuiClientBuilder::default().build(&rpc).await?;
    for _ in 0..3 {
        fund(&faucet, address).await?;
    }
    wait_for_gas(&client, address).await?;

    // Publish the protocol.
    let (package, admin_cap) = publish_protocol(&client, &signer, &contracts_dir()).await?;
    eprintln!("protocol package = {package}, admin_cap = {admin_cap}");

    // Roll a 2-strike grid. U/S are phantom on the bucket; SUI is a fine
    // stand-in since this test only creates the buckets (no writes).
    let plan = RollPlan {
        underlying_symbol: "SUI".into(),
        settlement_symbol: "SUI".into(),
        underlying_type: "0x2::sui::SUI".into(),
        settlement_type: "0x2::sui::SUI".into(),
        underlying_decimals: 9,
        settlement_decimals: 9,
        expiry_ms: 7_000_000_000_000,
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
        signer,
        network: Network::Devnet,
    };
    let out = roller::submit(&wrap, package, admin_cap, &plan, None, GAS).await?;
    eprintln!("roll digest = {}, buckets = {:?}", out.digest, out.bucket_ids);
    assert_eq!(out.bucket_ids.len(), 2, "expected two buckets");

    // Each bucket must be a Bucket<U, S, Call_i> with three type params, and
    // the third (the option coin) must be distinct per bucket.
    let mut call_types = Vec::new();
    for bid in &out.bucket_ids {
        let obj = wrap
            .client
            .read_api()
            .get_object_with_options(*bid, SuiObjectDataOptions::new().with_type())
            .await?;
        let ot = obj
            .data
            .ok_or_else(|| anyhow!("bucket {bid} not found"))?
            .object_type()?;
        match ot {
            ObjectType::Struct(mot) => {
                let params = mot.type_params();
                assert_eq!(params.len(), 3, "Bucket should have 3 type params");
                let call = params[2].to_canonical_string(true);
                assert!(
                    call.contains("::call_"),
                    "3rd type param should be a generated call coin, got {call}"
                );
                call_types.push(call);
            }
            other => panic!("bucket object not a struct: {other}"),
        }
    }
    call_types.sort();
    call_types.dedup();
    assert_eq!(call_types.len(), 2, "each bucket must have a distinct coin type");

    eprintln!("✓ localnet roll created 2 buckets with distinct option coins: {call_types:?}");
    Ok(())
}
