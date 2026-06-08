//! Per-roll option-coin package: publish + `bucket::create_bucket`.
//!
//! The options-scheduler generates one Move package per bucket set, each
//! containing N One-Time-Witness coin modules (`call_0..call_{N-1}`). This
//! module publishes that compiled package, harvests the `TreasuryCap<Call_i>`
//! each module's `init` minted, and then — in a single PTB — calls
//! `bucket::create_bucket<U, S, Call_i>` once per cap so every strike gets its
//! own fungible option coin backed by a bucket-owned treasury.
//!
//! Two transactions, not one: a package's `init` transfers its caps to the
//! sender, and objects created inside a `Publish` command aren't addressable
//! as results in the same PTB — so we publish, read the caps back from the
//! effects, then create the buckets.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, ObjectRef};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, Transaction, TransactionData};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::owned_object_arg;

/// A `TreasuryCap<Call>` harvested from a publish, paired with the Call type
/// it mints. `call_type` is the fully-qualified type string
/// (`0x<pkg>::call_<i>::CALL_<I>`), suitable for a `create_bucket` type arg.
#[derive(Debug, Clone)]
pub struct HarvestedCap {
    pub call_type: String,
    pub cap_ref: ObjectRef,
}

/// Result of publishing a generated coin package.
pub struct CoinPackagePublish {
    pub package_id: ObjectID,
    pub digest: String,
    /// One per `call_*` module, in no particular order. Pair to strikes by
    /// parsing the module index out of `call_type` (see scheduler `roller`).
    pub caps: Vec<HarvestedCap>,
}

/// Publish a compiled coin package (raw module bytes + dependency ids) and
/// harvest every `TreasuryCap<_>` its module inits created.
pub async fn publish_coin_package(
    client: &SuiClient,
    signer: &Signer,
    modules: Vec<Vec<u8>>,
    deps: Vec<ObjectID>,
    gas_budget: u64,
) -> Result<CoinPackagePublish> {
    info!(modules = modules.len(), deps = deps.len(), "publishing coin package");
    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building coin-package publish tx")?;
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
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
        .context("submitting coin-package publish tx")?;
    assert_success(&resp, "coin-package publish")?;
    parse_publish(&resp)
}

fn parse_publish(resp: &SuiTransactionBlockResponse) -> Result<CoinPackagePublish> {
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("coin-package publish: response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut caps: Vec<HarvestedCap> = Vec::new();

    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => package_id = Some(*pid),
            ObjectChange::Created {
                object_id,
                object_type,
                version,
                digest: obj_digest,
                ..
            } => {
                // TreasuryCap<Call> — 0x2::coin::TreasuryCap with the Call
                // type as its sole type parameter.
                if object_type.module.as_str() == "coin"
                    && object_type.name.as_str() == "TreasuryCap"
                {
                    let call_tag = object_type
                        .type_params
                        .first()
                        .ok_or_else(|| anyhow!("TreasuryCap with no type param"))?;
                    caps.push(HarvestedCap {
                        call_type: call_tag.to_canonical_string(/* with_prefix */ true),
                        cap_ref: (*object_id, *version, *obj_digest),
                    });
                }
            }
            _ => {}
        }
    }

    let package_id =
        package_id.ok_or_else(|| anyhow!("coin-package publish: no Published change"))?;
    if caps.is_empty() {
        return Err(anyhow!("coin-package publish: no TreasuryCap objects created"));
    }
    debug!(%package_id, caps = caps.len(), "coin package published");
    Ok(CoinPackagePublish { package_id, digest, caps })
}

/// One bucket to create: its asset triple, the cap that backs its option
/// coin, and its on-chain parameters.
pub struct CreateBucketSpec {
    pub underlying_type: String,
    pub settlement_type: String,
    pub call_type: String,
    pub cap_ref: ObjectRef,
    pub expiry_ms: u64,
    pub strike: u128,
    pub strike_scale: u8,
}

/// Call `bucket::create_bucket<U, S, Call>` once per spec in a single PTB,
/// consuming each `TreasuryCap` by value and referencing the shared `AdminCap`
/// across every command.
pub async fn create_buckets(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    specs: &[CreateBucketSpec],
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    if specs.is_empty() {
        return Err(anyhow!("create_buckets called with no specs"));
    }
    info!(%package, buckets = specs.len(), "building create_buckets PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    // AdminCap is an owned object passed by `&AdminCap`; input it once and
    // reuse the Argument across every create_bucket command.
    let admin_arg = pt.obj(owned_object_arg(client, admin_cap).await?)?;

    let bucket_module = Identifier::new("bucket").unwrap();
    let create_fn = Identifier::new("create_bucket").unwrap();

    for spec in specs {
        let u_tag = TypeTag::from_str(&spec.underlying_type)
            .with_context(|| format!("parsing underlying type {}", spec.underlying_type))?;
        let s_tag = TypeTag::from_str(&spec.settlement_type)
            .with_context(|| format!("parsing settlement type {}", spec.settlement_type))?;
        let c_tag = TypeTag::from_str(&spec.call_type)
            .with_context(|| format!("parsing call type {}", spec.call_type))?;

        let cap_arg = pt.obj(ObjectArg::ImmOrOwnedObject(spec.cap_ref))?;
        let expiry_arg = pt.pure(&spec.expiry_ms)?;
        let strike_arg = pt.pure(&spec.strike)?;
        let scale_arg = pt.pure(&spec.strike_scale)?;

        pt.programmable_move_call(
            package,
            bucket_module.clone(),
            create_fn.clone(),
            vec![u_tag, s_tag, c_tag],
            vec![admin_arg, cap_arg, expiry_arg, strike_arg, scale_arg],
        );
    }

    let programmable = pt.finish();

    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(10))
        .await
        .context("listing gas coins")?
        .data
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no SUI coins to pay gas for {}", signer.address))?;
    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;

    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin.object_ref()],
        programmable,
        gas_budget,
        gas_price,
    );
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
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
        .context("submitting create_buckets tx")?;
    assert_success(&resp, "create_buckets")?;
    debug!(digest = %resp.digest, "create_buckets succeeded");
    Ok(resp)
}

fn assert_success(resp: &SuiTransactionBlockResponse, what: &str) -> Result<()> {
    let effects = resp
        .effects
        .as_ref()
        .ok_or_else(|| anyhow!("{what}: response missing effects"))?;
    if effects.status().is_err() {
        return Err(anyhow!("{what} reverted: {:?}", effects.status()));
    }
    Ok(())
}
