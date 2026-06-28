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

use anyhow::{anyhow, bail, Context, Result};
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
use sui_types::transaction::{Argument, Command, ObjectArg, Transaction, TransactionData};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::{owned_object_arg, shared_object_arg};

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

/// DeepBook pool-creation parameters for a roll (SO-173). When passed to
/// [`create_buckets_and_pools`], every bucket's call coin also gets a
/// permissionless pool against the settlement asset in the SAME PTB — so
/// buckets and pools land atomically (a pool failure rolls the whole family
/// back, and the scheduler re-rolls next tick). All pools in a roll share one
/// tick/lot/min grid because they share base (call, = underlying decimals) and
/// quote (settlement) decimals.
pub struct PoolCreation {
    /// Upgraded DeepBook package — pool calls target this.
    pub deepbook_package: ObjectID,
    /// Shared DeepBook `Registry`.
    pub registry: ObjectID,
    /// DEEP coin type the creation fee is paid in.
    pub deep_coin_type: String,
    /// Creation fee per pool, in DEEP atomic units.
    pub fee: u64,
    pub tick: u64,
    pub lot: u64,
    pub min: u64,
}

/// Call `bucket::create_bucket<U, S, Call>` once per spec in a single PTB,
/// consuming each `TreasuryCap` by value and referencing the shared `AdminCap`
/// across every command. When `pools` is `Some`, the same PTB also calls
/// `pool::create_permissionless_pool<Call, S>` for each bucket — buckets and
/// pools are created atomically (SO-173).
pub async fn create_buckets_and_pools(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    specs: &[CreateBucketSpec],
    pools: Option<&PoolCreation>,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    create_buckets_impl(
        client, signer, package, admin_cap, specs, pools, gas_budget, "bucket", "create_bucket",
    )
    .await
}

/// Cash-secured-put twin of [`create_buckets_and_pools`]: calls
/// `put_bucket::create_put_bucket<U, S, Put>` per spec (the `call_type` field
/// of [`CreateBucketSpec`] holds the put coin type). Pools, if requested, are
/// `Pool<Put, Settlement>` — identical grid logic.
pub async fn create_put_buckets_and_pools(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    specs: &[CreateBucketSpec],
    pools: Option<&PoolCreation>,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    create_buckets_impl(
        client, signer, package, admin_cap, specs, pools, gas_budget, "put_bucket",
        "create_put_bucket",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_buckets_impl(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    specs: &[CreateBucketSpec],
    pools: Option<&PoolCreation>,
    gas_budget: u64,
    bucket_module_name: &str,
    create_fn_name: &str,
) -> Result<SuiTransactionBlockResponse> {
    if specs.is_empty() {
        return Err(anyhow!("create_buckets called with no specs"));
    }
    info!(
        %package,
        buckets = specs.len(),
        pools = pools.is_some(),
        "building create_buckets PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // AdminCap is an owned object passed by `&AdminCap`; input it once and
    // reuse the Argument across every create_bucket command.
    let admin_arg = pt.obj(owned_object_arg(client, admin_cap).await?)?;

    // Pool prelude: the shared registry, the (roll-wide) grid params, and one
    // DEEP fee coin per pool — all set up once before the per-strike loop.
    let pool_ctx = match pools {
        Some(p) => {
            let registry = pt.obj(shared_object_arg(client, p.registry, true).await?)?;
            let tick = pt.pure(&p.tick)?;
            let lot = pt.pure(&p.lot)?;
            let min = pt.pure(&p.min)?;
            let fee_coins =
                split_deep_fees(client, signer, &mut pt, &p.deep_coin_type, p.fee, specs.len())
                    .await?;
            Some((p.deepbook_package, registry, tick, lot, min, fee_coins))
        }
        None => None,
    };

    let bucket_module = Identifier::new(bucket_module_name)
        .map_err(|e| anyhow!("bucket module {bucket_module_name}: {e}"))?;
    let create_fn = Identifier::new(create_fn_name)
        .map_err(|e| anyhow!("create fn {create_fn_name}: {e}"))?;
    let pool_module = Identifier::new("pool").unwrap();
    let create_pool_fn = Identifier::new("create_permissionless_pool").unwrap();

    for (i, spec) in specs.iter().enumerate() {
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
            vec![u_tag, s_tag.clone(), c_tag.clone()],
            vec![admin_arg, cap_arg, expiry_arg, strike_arg, scale_arg],
        );

        // Same PTB: create this call coin's DeepBook pool (base = call, quote =
        // settlement), paid from the i-th split DEEP fee coin.
        if let Some((db_pkg, registry, tick, lot, min, fee_coins)) = &pool_ctx {
            pt.programmable_move_call(
                *db_pkg,
                pool_module.clone(),
                create_pool_fn.clone(),
                vec![c_tag, s_tag],
                vec![*registry, *tick, *lot, *min, fee_coins[i]],
            );
        }
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

/// Merge the signer's DEEP coins and split off `count` coins of exactly `fee`
/// each — one per pool-creation call in the same PTB. Returns the per-pool coin
/// Arguments (the `NestedResult`s of the SplitCoins command).
async fn split_deep_fees(
    client: &SuiClient,
    signer: &Signer,
    pt: &mut ProgrammableTransactionBuilder,
    deep_coin_type: &str,
    fee: u64,
    count: usize,
) -> Result<Vec<Argument>> {
    let coins = client
        .coin_read_api()
        .get_coins(signer.address, Some(deep_coin_type.to_string()), None, Some(50))
        .await
        .with_context(|| format!("listing {deep_coin_type} coins"))?
        .data;
    let total: u128 = coins.iter().map(|c| c.balance as u128).sum();
    let need = fee as u128 * count as u128;
    if total < need {
        bail!("wallet holds {total} of {deep_coin_type}, need {need} for {count} pool fees");
    }
    let mut refs = coins.into_iter().map(|c| c.object_ref());
    let first = refs.next().ok_or_else(|| anyhow!("no {deep_coin_type} coins"))?;
    let primary = pt.obj(ObjectArg::ImmOrOwnedObject(first))?;
    let rest: Vec<Argument> = refs
        .map(|r| pt.obj(ObjectArg::ImmOrOwnedObject(r)))
        .collect::<Result<_, _>>()?;
    if !rest.is_empty() {
        pt.command(Command::MergeCoins(primary, rest));
    }
    // One amount Argument reused `count` times (pure inputs aren't consumed).
    let amt = pt.pure(&fee)?;
    let split = pt.command(Command::SplitCoins(primary, vec![amt; count]));
    let base = match split {
        Argument::Result(i) => i,
        other => bail!("SplitCoins returned unexpected argument {other:?}"),
    };
    Ok((0..count as u16).map(|j| Argument::NestedResult(base, j)).collect())
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
