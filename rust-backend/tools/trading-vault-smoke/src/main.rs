//! End-to-end smoke of the curated trading vault on a live network
//! (SO-292): create a vault → deposit → DeepBook custody → resting order
//! through the wrapped BalanceManager → appraised deposit with the
//! custody live → cancel/sweep/withdraw → queue + fulfill → assert the
//! depositor got every unit back. Exercises vault core, the DeepBook
//! adapter, the appraisal composer, and the withdrawal queue against
//! REAL deployed contracts.
//!
//!   cargo run -p trading-vault-smoke -- \
//!     --address 0xab8d… [--rpc …] [--env staging] [--indexer-graphql …]
//!
//! Signs with the local sui keystore (the deployer key). Costs gas plus
//! nothing: every token unit round-trips back to the vault depositor.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::{ObjectChange, SuiObjectDataOptions, SuiTransactionBlockResponseOptions};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::SuiKeyPair;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use sui_tx::sui_client::Signer;
use sui_tx::tx::appraisal::{
    compose_appraisal, discover_holdings, pyth_assets_needed, AppraisalRefs, OptionBucketInfo,
    PriceLegs,
};
use sui_tx::tx::pyth_update::PythHandles;
use sui_tx::tx::deepbook::derived_pool_params;
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

#[derive(Parser)]
struct Cli {
    /// Signer address; its key must be in the local sui keystore.
    #[arg(long)]
    address: String,
    #[arg(long, default_value = "https://sui-testnet-rpc.publicnode.com")]
    rpc: String,
    #[arg(long, default_value = "staging")]
    env: String,
    #[arg(long, default_value = "deployments.json")]
    deployments: PathBuf,
    #[arg(long, default_value = "https://sui-options.com/staging/indexer/graphql")]
    indexer_graphql: String,
    #[arg(long, default_value_t = 1_000_000)]
    deposit_amount: u64,
    #[arg(long, default_value_t = 100_000_000)]
    gas_budget: u64,
    /// Skip the DeepBook leg (vault-core-only smoke).
    #[arg(long, default_value_t = false)]
    skip_deepbook: bool,
    /// Stop after create/deposit/custody-fund and leave the vault live
    /// (for pointing a vault-mode mm-bot at it). Prints the ids to keep.
    #[arg(long, default_value_t = false)]
    keep_open: bool,
    /// Fill the vault's resting bid with self-written calls (SO-297): the
    /// custody ends up holding a nonzero option-coin balance, the deposit
    /// and a partial fulfillment run through `options_oracle` legs, and
    /// the vault is left OPEN (the live MM-vault state) instead of drained.
    #[arg(long, default_value_t = false)]
    fill_bid: bool,
}

fn load_signer(address: &str) -> Result<Signer> {
    let address = SuiAddress::from_str(address).context("parsing --address")?;
    let path = dirs_home().join(".sui/sui_config/sui.keystore");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading keystore {}", path.display()))?;
    let entries: Vec<String> = serde_json::from_str(&raw).context("parsing keystore json")?;
    for entry in entries {
        use sui_types::crypto::EncodeDecodeBase64;
        if let Ok(kp) = SuiKeyPair::decode_base64(&entry) {
            let derived = SuiAddress::from(&kp.public());
            if derived == address {
                return Ok(Signer { keypair: kp, address });
            }
        }
    }
    bail!("no key for {address} in {}", path.display())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

struct Ids {
    trading_vault_pkg: ObjectID,
    oracle_pyth_pkg: ObjectID,
    deepbook_adapter_pkg: ObjectID,
    options_adapter_pkg: ObjectID,
    tokens_pkg: ObjectID,
    protocol_config_id: ObjectID,
    integration_registry_id: ObjectID,
    oracle_registry_id: ObjectID,
    pyth_feed_registry_id: ObjectID,
    pool_allowlist_id: ObjectID,
    vol_book_id: Option<ObjectID>,
    treasury_id: ObjectID,
    deposit_coin_type: String,
    deposit_module: String,
    deposit_faucet: ObjectID,
}

async fn resolve_ids(client: &SuiClient, cli: &Cli) -> Result<Ids> {
    let deps = deployments::Deployments::load(&cli.deployments)
        .context("loading deployments.json")?;
    let net = deps.for_env(&cli.env)?;
    let pi = &net.package_info;
    let tv = pi.trading_vault.as_ref().ok_or_else(|| anyhow!("no tradingVault record"))?;
    let op = pi.oracle_pyth.as_ref().ok_or_else(|| anyhow!("no oraclePyth record"))?;
    let dba = pi
        .deepbook_adapter
        .as_ref()
        .ok_or_else(|| anyhow!("no deepbookAdapter record"))?;
    let oa = pi
        .options_adapter
        .as_ref()
        .ok_or_else(|| anyhow!("no optionsAdapter record"))?;
    let tt = pi.test_tokens.as_ref().ok_or_else(|| anyhow!("no testTokens record"))?;
    let tusdc = tt
        .tokens
        .get("TUSDC")
        .ok_or_else(|| anyhow!("no TUSDC test token"))?;

    // Governance ids: recorded block first, publish-effects fallback.
    let (pc, ireg, oreg, freg, plist) = if let Some(objs) = pi.trading_vault_objects.as_ref() {
        (
            objs.vault_protocol_config()?,
            objs.integration_registry()?,
            objs.oracle_registry()?,
            objs.pyth_feed_registry()?,
            objs.pool_allowlist()?,
        )
    } else {
        let tv_created = created_map(client, &tv.publish_digest).await?;
        let op_created = created_map(client, &op.publish_digest).await?;
        let dba_created = created_map(client, &dba.publish_digest).await?;
        let pick = |m: &BTreeMap<String, ObjectID>, k: &str| {
            m.get(k).copied().ok_or_else(|| anyhow!("{k} missing from publish effects"))
        };
        (
            pick(&tv_created, "registry::VaultProtocolConfig")?,
            pick(&tv_created, "registry::IntegrationRegistry")?,
            pick(&tv_created, "registry::OracleRegistry")?,
            pick(&op_created, "oracle_pyth::PythFeedRegistry")?,
            pick(&dba_created, "deepbook_adapter::PoolAllowlist")?,
        )
    };

    Ok(Ids {
        trading_vault_pkg: tv.package()?,
        oracle_pyth_pkg: op.package()?,
        deepbook_adapter_pkg: dba.package()?,
        options_adapter_pkg: oa.package()?,
        tokens_pkg: ObjectID::from_hex_literal(&tt.package_id)?,
        protocol_config_id: pc,
        integration_registry_id: ireg,
        oracle_registry_id: oreg,
        pyth_feed_registry_id: freg,
        pool_allowlist_id: plist,
        vol_book_id: pi
            .trading_vault_objects
            .as_ref()
            .map(|o| o.vol_book())
            .transpose()?
            .flatten(),
        treasury_id: net.treasury()?,
        deposit_coin_type: protocol_types::asset::canonicalize_move_type(&tusdc.coin_type),
        deposit_module: "tusdc".into(),
        deposit_faucet: tusdc.faucet()?,
    })
}

async fn created_map(client: &SuiClient, digest: &str) -> Result<BTreeMap<String, ObjectID>> {
    use sui_json_rpc_types::SuiTransactionBlockEffectsAPI;
    let resp = client
        .read_api()
        .get_transaction_with_options(
            digest.parse()?,
            SuiTransactionBlockResponseOptions::new().with_effects(),
        )
        .await?;
    let ids: Vec<ObjectID> = resp
        .effects
        .as_ref()
        .map(|e| e.created().iter().map(|c| c.reference.object_id).collect())
        .unwrap_or_default();
    let objs = client
        .read_api()
        .multi_get_object_with_options(ids, SuiObjectDataOptions::new().with_type())
        .await?;
    let mut out = BTreeMap::new();
    for o in objs {
        if let Some(d) = o.data {
            if let Some(t) = d.type_ {
                let full = t.to_string();
                if let Some((_, tail)) = full.rsplit_once("::").and_then(|(head, name)| {
                    head.rsplit_once("::").map(|(_, module)| ((), format!("{module}::{name}")))
                }) {
                    out.insert(tail, d.object_id);
                }
            }
        }
    }
    Ok(out)
}

/// One bucket row from the indexer, keyed both by pool and by option
/// coin type. The pool's BASE is the per-roll option coin, which inherits
/// the underlying's decimals — needed to re-derive the tick/lot grid.
#[derive(Debug, Clone)]
struct BucketRef {
    bucket_id: ObjectID,
    underlying: String,
    settlement: String,
}

/// (pool id → bucket, canonical option-coin type → OptionBucketInfo).
async fn fetch_bucket_catalog(
    url: &str,
) -> Result<(BTreeMap<String, BucketRef>, BTreeMap<String, OptionBucketInfo>)> {
    let body = serde_json::json!({
        "query": "{ buckets { bucketId assetType settlementType callType optionKind deepbookPoolId } }"
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let mut by_pool = BTreeMap::new();
    let mut by_coin = BTreeMap::new();
    for b in resp
        .pointer("/data/buckets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let get = |k: &str| b.pointer(&format!("/{k}")).and_then(|v| v.as_str());
        let (Some(bucket_id), Some(asset), Some(settlement), Some(call_type)) =
            (get("bucketId"), get("assetType"), get("settlementType"), get("callType"))
        else {
            continue;
        };
        let Ok(bucket_id) = ObjectID::from_hex_literal(bucket_id) else { continue };
        let is_put = get("optionKind") == Some("put");
        let r = BucketRef {
            bucket_id,
            underlying: protocol_types::asset::canonicalize_move_type(asset),
            settlement: protocol_types::asset::canonicalize_move_type(settlement),
        };
        if let Some(pool) = get("deepbookPoolId") {
            by_pool.insert(canon_id(pool), r.clone());
        }
        by_coin.insert(
            protocol_types::asset::canonicalize_move_type(call_type),
            OptionBucketInfo {
                bucket_id: r.bucket_id,
                underlying: r.underlying.clone(),
                settlement: r.settlement.clone(),
                is_put,
            },
        );
    }
    Ok((by_pool, by_coin))
}

fn canon_id(s: &str) -> String {
    let hex = s.trim_start_matches("0x").to_ascii_lowercase();
    format!("0x{hex:0>64}")
}

struct Step(&'static str);
impl Step {
    fn ok(self) {
        println!("  ✔ {}", self.0);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info,sui_tx=warn").init();
    let cli = Cli::parse();
    let client = SuiClientBuilder::default().build(&cli.rpc).await?;
    let signer = load_signer(&cli.address)?;
    let ids = resolve_ids(&client, &cli).await?;
    println!("trading-vault smoke — signer {}, env {}", signer.address, cli.env);

    // ── 1. create vault (deposit asset TUSDC, self as curator, no lockup)
    let step = Step("create_vault");
    let mut pt = ProgrammableTransactionBuilder::new();
    let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
    let curator = pt.pure(signer.address)?;
    let a0 = pt.pure(0u64)?; // lockup
    let a1 = pt.pure(1_000u64)?; // curator fee bps
    let a2 = pt.pure(2u8)?; // ROTATE_EITHER
    let a3 = pt.pure(8u64)?; // max positions
    let a4 = pt.pure(3_600_000u64)?; // unwind grace
    let deposit_tag = TypeTag::from_str(&ids.deposit_coin_type)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("create_vault").unwrap(),
        vec![deposit_tag.clone()],
        vec![cfg, curator, a0, a1, a2, a3, a4],
    );
    let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::create_vault").await?;
    let (mut vault_id, mut cap_id) = (None, None);
    for c in resp.object_changes.iter().flatten() {
        if let ObjectChange::Created { object_id, object_type, .. } = c {
            match object_type.name.as_str() {
                "TradingVault" => vault_id = Some(*object_id),
                "CuratorCap" => cap_id = Some(*object_id),
                _ => {}
            }
        }
    }
    let vault_id = vault_id.ok_or_else(|| anyhow!("vault not created"))?;
    let cap_id = cap_id.ok_or_else(|| anyhow!("curator cap not created"))?;
    step.ok();
    println!("    vault {vault_id}");

    // ── 2. faucet-mint + deposit
    let step = Step("deposit (cash-only appraisal)");
    let refs = AppraisalRefs {
        trading_vault_pkg: ids.trading_vault_pkg,
        oracle_pyth_pkg: ids.oracle_pyth_pkg,
        deepbook_adapter_pkg: Some(ids.deepbook_adapter_pkg),
        options_adapter_pkg: Some(ids.options_adapter_pkg),
        vault_id,
        protocol_config_id: ids.protocol_config_id,
        oracle_registry_id: ids.oracle_registry_id,
        pyth_feed_registry_id: ids.pyth_feed_registry_id,
        // SO-299: the smoke vault has no external account.
        equity_oracle_pkg: None,
        equity_book_id: None,
        vol_book_id: ids.vol_book_id,
        dbm: None,
    };
    let http = reqwest::Client::new();
    let (pool_buckets, option_map) = match fetch_bucket_catalog(&cli.indexer_graphql).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("    (indexer bucket lookup failed: {e:#})");
            (BTreeMap::new(), BTreeMap::new())
        }
    };
    let deps_top = deployments::Deployments::load(&cli.deployments)?;
    let net_top = deps_top.for_env(&cli.env)?;
    let mut feeds_by_type: BTreeMap<String, protocol_types::PriceFeedId> = BTreeMap::new();
    for t in net_top.token_info.values() {
        if let Ok(f) = t.pyth_feed() {
            feeds_by_type
                .insert(protocol_types::asset::canonicalize_move_type(&t.coin_type), f);
        }
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(&client, vault_id).await?;
    let appraisal =
        compose_appraisal(&client, &mut pt, &refs, &holdings, None, &BTreeMap::new()).await?;
    let faucet = pt.obj(shared_object_arg(&client, ids.deposit_faucet, true).await?)?;
    let amount = pt.pure(cli.deposit_amount)?;
    let coin = pt.programmable_move_call(
        ids.tokens_pkg,
        Identifier::new(&*ids.deposit_module).unwrap(),
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount],
    );
    let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![deposit_tag.clone()],
        vec![vault_arg, cfg, appraisal, coin, clock],
    );
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit").await?;
    step.ok();

    let mut custody: Option<(ObjectID, ObjectID, String, String)> = None;
    if !cli.skip_deepbook {
        // ── 3. custody + resting order on an allowlisted pool
        let step = Step("init_custody + fund");
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cap = pt.obj(sui_tx::tx::owned_object_arg(&client, cap_id).await?)?;
        let ireg = pt.obj(shared_object_arg(&client, ids.integration_registry_id, false).await?)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("init_custody").unwrap(),
            vec![],
            vec![vault_arg, cap, ireg],
        );
        let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::init_custody").await?;
        let custody_id = resp
            .object_changes
            .iter()
            .flatten()
            .find_map(|c| match c {
                ObjectChange::Created { object_id, object_type, .. }
                    if object_type.name.as_str() == "DeepBookCustody" =>
                {
                    Some(*object_id)
                }
                _ => None,
            })
            .ok_or_else(|| anyhow!("custody not created"))?;

        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cap = pt.obj(sui_tx::tx::owned_object_arg(&client, cap_id).await?)?;
        let ireg = pt.obj(shared_object_arg(&client, ids.integration_registry_id, false).await?)?;
        let custody_arg = pt.pure(custody_id)?;
        let fund = pt.pure(cli.deposit_amount / 2)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cap, ireg, custody_arg, fund],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::custody_fund").await?;
        step.ok();

        if cli.keep_open {
            println!("\nvault left open:");
            println!("    vault_id       = \"{vault_id}\"");
            println!("    curator_cap_id = \"{cap_id}\"");
            println!("    custody_id     = \"{custody_id}\"");
            return Ok(());
        }

        // Pick an allowlisted pool quoted in the deposit asset.
        let step = Step("place resting bid through wrapped BM");
        let allow = client
            .read_api()
            .get_object_with_options(
                ids.pool_allowlist_id,
                SuiObjectDataOptions::new().with_content(),
            )
            .await?;
        let allow_json = serde_json::to_value(
            allow.data.and_then(|d| d.content).ok_or_else(|| anyhow!("allowlist unreadable"))?,
        )?;
        let pool_ids: Vec<String> = allow_json
            .pointer("/fields/allowed/fields/contents")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let deps = deployments::Deployments::load(&cli.deployments)?;
        let net = deps.for_env(&cli.env)?;
        let decimals_of = |coin_type: &str| -> Option<u8> {
            net.token_info.values().find_map(|t| {
                (protocol_types::asset::canonicalize_move_type(&t.coin_type) == coin_type)
                    .then_some(t.decimals)
            })
        };
        let mut picked = None;
        for pid_str in &pool_ids {
            let pid = ObjectID::from_hex_literal(pid_str)?;
            let t = client
                .read_api()
                .get_object_with_options(pid, SuiObjectDataOptions::new().with_type())
                .await?
                .data
                .and_then(|d| d.type_)
                .map(|t| t.to_string())
                .unwrap_or_default();
            if let Some((_, inner)) = t.split_once('<') {
                let inner = inner.trim_end_matches('>');
                let parts: Vec<&str> = inner.splitn(2, ',').map(str::trim).collect();
                if parts.len() != 2
                    || protocol_types::asset::canonicalize_move_type(parts[1])
                        != ids.deposit_coin_type
                {
                    eprintln!(
                        "    skip {pid_str}: quote {} != deposit {}",
                        parts.get(1).unwrap_or(&"?"),
                        ids.deposit_coin_type
                    );
                    continue;
                }
                // Grid derivation needs the underlying's decimals.
                let Some(bref) = pool_buckets.get(&canon_id(pid_str)) else {
                    eprintln!("    skip {pid_str}: not in indexer map");
                    continue;
                };
                let Some(base_decimals) = decimals_of(&bref.underlying) else {
                    eprintln!("    skip {pid_str}: no decimals for {}", bref.underlying);
                    continue;
                };
                let deepbook_pkg = ObjectID::from_hex_literal(
                    t.split("::").next().unwrap_or_default(),
                )
                .context("parsing deepbook package from pool type")?;
                picked =
                    Some((pid, parts[0].to_string(), parts[1].to_string(), base_decimals, deepbook_pkg));
                break;
            }
        }
        let (pool_id, base_ty, quote_ty, base_decimals, deepbook_pkg) = picked.ok_or_else(|| {
            anyhow!("no allowlisted pool with a resolvable underlying — is the indexer reachable?")
        })?;
        let quote_decimals =
            decimals_of(&ids.deposit_coin_type).ok_or_else(|| anyhow!("deposit decimals"))?;
        let (tick, lot, min) = derived_pool_params(base_decimals, quote_decimals);
        let qty = min.max(lot);
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cap = pt.obj(sui_tx::tx::owned_object_arg(&client, cap_id).await?)?;
        let ireg = pt.obj(shared_object_arg(&client, ids.integration_registry_id, false).await?)?;
        let list = pt.obj(shared_object_arg(&client, ids.pool_allowlist_id, false).await?)?;
        let custody_arg = pt.pure(custody_id)?;
        let pool = pt.obj(shared_object_arg(&client, pool_id, true).await?)?;
        let args = [
            pt.pure(4242u64)?,  // client order id
            pt.pure(0u8)?,      // no restriction
            pt.pure(0u8)?,      // self-matching allowed
            pt.pure(tick)?,     // minimal price: one tick
            pt.pure(qty)?,
            pt.pure(true)?,     // bid
            pt.pure(false)?,    // pay_with_deep
            pt.pure(u64::MAX)?, // expire
        ];
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("place_limit_order").unwrap(),
            vec![TypeTag::from_str(&base_ty)?, TypeTag::from_str(&quote_ty)?],
            vec![
                vault_arg, cap, ireg, list, custody_arg, pool, args[0], args[1], args[2], args[3],
                args[4], args[5], args[6], args[7], clock,
            ],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::place_limit_order").await?;
        step.ok();
        println!("    pool {pool_id}");
        custody = Some((custody_id, pool_id, base_ty.clone(), quote_ty.clone()));

        if cli.fill_bid {
            // ── 3b. cross the vault's bid with self-written calls: mint
            // underlying, write into the bucket (coins + Position to this
            // wallet), market-sell into the resting bid from a throwaway
            // BalanceManager, then crank the maker-side settlement so the
            // custody's CALL balance is live.
            let step = Step("fill the vault bid (self-write + market sell)");
            let bref = pool_buckets
                .get(&canon_id(&pool_id.to_string()))
                .ok_or_else(|| anyhow!("picked pool missing from bucket catalog"))?;
            let (u_module, u_faucet) = {
                let tt = deps
                    .for_env(&cli.env)?
                    .package_info
                    .test_tokens
                    .clone()
                    .ok_or_else(|| anyhow!("no testTokens record"))?;
                tt.tokens
                    .iter()
                    .find(|(_, t)| {
                        protocol_types::asset::canonicalize_move_type(&t.coin_type)
                            == bref.underlying
                    })
                    .map(|(sym, t)| (sym.to_lowercase(), t.faucet()))
                    .ok_or_else(|| anyhow!("underlying {} not in test tokens", bref.underlying))?
            };
            let u_faucet = u_faucet?;
            let write_amt = qty * 3; // headroom for input-token taker fees
            let mut pt = ProgrammableTransactionBuilder::new();
            let faucet = pt.obj(shared_object_arg(&client, u_faucet, true).await?)?;
            let amount = pt.pure(write_amt)?;
            let minted = pt.programmable_move_call(
                ids.tokens_pkg,
                Identifier::new(&*u_module).unwrap(),
                Identifier::new("mint").unwrap(),
                vec![],
                vec![faucet, amount],
            );
            let bucket = pt.obj(shared_object_arg(&client, bref.bucket_id, true).await?)?;
            let clock = clock_arg(&mut pt)?;
            let tags = vec![
                TypeTag::from_str(&bref.underlying)?,
                TypeTag::from_str(&bref.settlement)?,
                TypeTag::from_str(&base_ty)?,
            ];
            let write_out = pt.programmable_move_call(
                net.package()?,
                Identifier::new("bucket").unwrap(),
                Identifier::new("write_collateralized").unwrap(),
                tags,
                vec![bucket, minted, clock],
            );
            let sui_types::transaction::Argument::Result(wi) = write_out else {
                bail!("unexpected write_collateralized result shape");
            };
            let position = sui_types::transaction::Argument::NestedResult(wi, 0);
            let call_coin = sui_types::transaction::Argument::NestedResult(wi, 1);
            pt.transfer_arg(signer.address, position);
            let bm = pt.programmable_move_call(
                deepbook_pkg,
                Identifier::new("balance_manager").unwrap(),
                Identifier::new("new").unwrap(),
                vec![],
                vec![],
            );
            pt.programmable_move_call(
                deepbook_pkg,
                Identifier::new("balance_manager").unwrap(),
                Identifier::new("deposit").unwrap(),
                vec![TypeTag::from_str(&base_ty)?],
                vec![bm, call_coin],
            );
            let proof = pt.programmable_move_call(
                deepbook_pkg,
                Identifier::new("balance_manager").unwrap(),
                Identifier::new("generate_proof_as_owner").unwrap(),
                vec![],
                vec![bm],
            );
            let pool = pt.obj(shared_object_arg(&client, pool_id, true).await?)?;
            let a_client = pt.pure(4243u64)?;
            let a_self = pt.pure(0u8)?;
            let a_qty = pt.pure(qty)?;
            let a_bid = pt.pure(false)?;
            let a_deep = pt.pure(false)?;
            pt.programmable_move_call(
                deepbook_pkg,
                Identifier::new("pool").unwrap(),
                Identifier::new("place_market_order").unwrap(),
                vec![TypeTag::from_str(&base_ty)?, TypeTag::from_str(&quote_ty)?],
                vec![pool, bm, proof, a_client, a_self, a_qty, a_bid, a_deep, clock],
            );
            pt.transfer_arg(signer.address, bm);
            submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fill_bid").await?;

            // Maker-side settlement: permissionless crank sweeps the bought
            // calls into the custody manager + tracks the asset type.
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
            let ireg =
                pt.obj(shared_object_arg(&client, ids.integration_registry_id, false).await?)?;
            let custody_arg = pt.pure(custody_id)?;
            let pool = pt.obj(shared_object_arg(&client, pool_id, true).await?)?;
            pt.programmable_move_call(
                ids.deepbook_adapter_pkg,
                Identifier::new("deepbook_adapter").unwrap(),
                Identifier::new("crank_withdraw_settled").unwrap(),
                vec![TypeTag::from_str(&base_ty)?, TypeTag::from_str(&quote_ty)?],
                vec![vault_arg, ireg, custody_arg, pool],
            );
            submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::crank_settled").await?;

            let call_bal = custody_coin_balance(
                &client,
                signer.address,
                ids.trading_vault_pkg,
                ids.deepbook_adapter_pkg,
                vault_id,
                custody_id,
                &base_ty,
            )
            .await?;
            if call_bal == 0 {
                bail!("custody holds no {base_ty} after the fill — sweep failed?");
            }
            step.ok();
            println!("    custody now holds {call_bal} {}", &base_ty[..14.min(base_ty.len())]);
        }

        // ── 4. deposit again WITH the custody live (appraisal covers it)
        let step = Step("deposit with live custody (composed appraisal)");
        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let appraisal = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
        )
        .await?;
        let faucet = pt.obj(shared_object_arg(&client, ids.deposit_faucet, true).await?)?;
        let amount = pt.pure(cli.deposit_amount)?;
        let coin = pt.programmable_move_call(
            ids.tokens_pkg,
            Identifier::new(&*ids.deposit_module).unwrap(),
            Identifier::new("mint").unwrap(),
            vec![],
            vec![faucet, amount],
        );
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cfg, appraisal, coin, clock],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit_with_custody").await?;
        step.ok();
    }

    if cli.fill_bid {
        // ── 5'. fill-bid mode: crystallize a PARTIAL withdrawal through the
        // option-coin appraisal, then leave the vault OPEN with its CALL
        // inventory — the live MM-vault state. Full-drain coverage lives in
        // the default (no-fill) mode.
        let step = Step("partial withdraw + fulfill (option-coin appraisal)");
        let total_shares = read_total_shares(&client, vault_id).await?;
        let half = total_shares / 2;
        if half == 0 {
            bail!("no shares to withdraw");
        }
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let shares = pt.pure(half)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("request_withdraw").unwrap(),
            vec![],
            vec![vault_arg, shares, clock],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw_half").await?;

        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let appraisal = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
        )
        .await?;
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
        let treasury = pt.obj(shared_object_arg(&client, ids.treasury_id, true).await?)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("fulfill_withdrawals").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cfg, treasury, appraisal],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fulfill_half").await?;
        let after = read_total_shares(&client, vault_id).await?;
        if after != total_shares - half {
            bail!("share supply {after} != expected {}", total_shares - half);
        }
        step.ok();
        println!(
            "\nOPTION-LEG SMOKE PASSED — vault {vault_id} deposited + fulfilled while holding \
             option coins; left open with {after} shares"
        );
        return Ok(());
    }

    // ── 5. unwind custody (cancel + withdraw funds back to the vault)
    if let Some((custody_id, pool_id, base_ty, quote_ty)) = &custody {
        let step = Step("cancel orders + withdraw custody funds");
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cap = pt.obj(sui_tx::tx::owned_object_arg(&client, cap_id).await?)?;
        let ireg = pt.obj(shared_object_arg(&client, ids.integration_registry_id, false).await?)?;
        let pool = pt.obj(shared_object_arg(&client, *pool_id, true).await?)?;
        let clock = clock_arg(&mut pt)?;
        let custody_arg = pt.pure(custody_id)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("cancel_all_orders").unwrap(),
            vec![TypeTag::from_str(base_ty)?, TypeTag::from_str(quote_ty)?],
            vec![vault_arg, cap, ireg, custody_arg, pool, clock],
        );
        let custody_arg = pt.pure(custody_id)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("retire_pool").unwrap(),
            vec![TypeTag::from_str(base_ty)?, TypeTag::from_str(quote_ty)?],
            vec![vault_arg, cap, ireg, custody_arg, pool],
        );
        let custody_arg = pt.pure(custody_id)?;
        let amount = pt.pure(cli.deposit_amount / 2)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            Identifier::new("deepbook_adapter").unwrap(),
            Identifier::new("withdraw").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cap, ireg, custody_arg, amount],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::unwind_custody").await?;
        step.ok();
    }

    // ── 6. request everything back + fulfill; the depositor's stake is
    // exactly the two deposits (no profit → no fee).
    let step = Step("request_withdraw + fulfill");
    let total = if cli.skip_deepbook { cli.deposit_amount } else { cli.deposit_amount * 2 };
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
    let shares = pt.pure(total as u128)?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("request_withdraw").unwrap(),
        vec![],
        vec![vault_arg, shares, clock],
    );
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw").await?;

    let holdings = discover_holdings(&client, vault_id).await?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let appraisal = compose_with_legs(
        &client,
        &http,
        &mut pt,
        &refs,
        &holdings,
        &option_map,
        &feeds_by_type,
    )
    .await?;
    let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(&client, ids.treasury_id, true).await?)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("fulfill_withdrawals").unwrap(),
        vec![deposit_tag.clone()],
        vec![vault_arg, cfg, treasury, appraisal],
    );
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fulfill").await?;
    step.ok();

    // ── 7. verify: vault drained, stake gone.
    let step = Step("verify vault drained");
    let holdings = discover_holdings(&client, vault_id).await?;
    // An EMPTY DeepBook custody legitimately remains (durable adapter
    // infrastructure, appraises at 0; removable via eject_empty_custody
    // before closure). Anything else is residual value.
    let residual = !holdings.free_assets.is_empty()
        || holdings.positions.iter().any(|p| {
            !matches!(
                p,
                sui_tx::tx::appraisal::PositionInfo::DeepBookCustody { assets, pools, .. }
                    if assets.is_empty() && pools.is_empty()
            )
        });
    if residual {
        bail!("vault still holds assets/positions after full exit: {holdings:?}");
    }
    step.ok();

    println!("\nSMOKE PASSED — vault {vault_id} exercised end to end");
    Ok(())
}

// ═══════════════════ option-coin appraisal legs (SO-297) ═══════════════════

/// Sui testnet Pyth + Wormhole deployment (same ids as the keeper's
/// `[pyth]` config and the frontend's PYTH_HANDLES).
fn pyth_handles() -> PythHandles {
    PythHandles {
        pyth_package: ObjectID::from_hex_literal(
            "0xabf837e98c26087cba0883c0a7a28326b1fa3c5e1e2c5abdb486f9e8f594c837",
        )
        .unwrap(),
        wormhole_package: ObjectID::from_hex_literal(
            "0xf47329f4344f3bf0f8e436e2f7b485466cff300f12a166563995d3888c296a94",
        )
        .unwrap(),
        pyth_state_id: ObjectID::from_hex_literal(
            "0x243759059f4c3111179da5878c12f68d612c21a8d54d85edc86164bb18be1c7c",
        )
        .unwrap(),
        wormhole_state_id: ObjectID::from_hex_literal(
            "0x31358d198147da50db32eda2562951d53973a0c0ad5ed738e9b17d88b213d790",
        )
        .unwrap(),
        update_fee_mist: 1,
    }
}

const HERMES_BETA: &str = "https://hermes-beta.pyth.network";

/// Pyth's feed → `PriceInfoObject` table (port of keeper::discovery).
struct PriceInfoTable {
    table_id: ObjectID,
    identifier_type: move_core_types::language_storage::TypeTag,
}

async fn resolve_price_info_table(client: &SuiClient, pyth_state_id: ObjectID) -> Result<PriceInfoTable> {
    use sui_types::dynamic_field::DynamicFieldName;
    let resp = client
        .read_api()
        .get_dynamic_field_object(
            pyth_state_id,
            DynamicFieldName {
                type_: TypeTag::from_str("vector<u8>").expect("static type tag"),
                value: serde_json::json!("price_info"),
            },
        )
        .await
        .context("reading pyth state price_info dynamic field")?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("pyth state {pyth_state_id} has no price_info table"))?;
    let table_id = data.object_id;
    let type_str = data
        .type_
        .as_ref()
        .map(|t| t.to_string())
        .ok_or_else(|| anyhow!("price_info table response missing type"))?;
    let key = type_str
        .split('<')
        .nth(1)
        .and_then(|inner| inner.split(',').next())
        .map(str::trim)
        .ok_or_else(|| anyhow!("unparseable price_info table type: {type_str}"))?;
    let identifier_type = TypeTag::from_str(key).with_context(|| format!("parsing {key}"))?;
    Ok(PriceInfoTable { table_id, identifier_type })
}

async fn price_info_object_for(
    client: &SuiClient,
    table: &PriceInfoTable,
    feed: protocol_types::PriceFeedId,
) -> Result<ObjectID> {
    use sui_types::dynamic_field::DynamicFieldName;
    let resp = client
        .read_api()
        .get_dynamic_field_object(
            table.table_id,
            DynamicFieldName {
                type_: table.identifier_type.clone(),
                value: serde_json::json!({ "bytes": feed.0.to_vec() }),
            },
        )
        .await
        .with_context(|| format!("looking up price info object for feed {feed}"))?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("feed {feed} has no PriceInfoObject on this network"))?;
    let content = data.content.ok_or_else(|| anyhow!("price info field missing content"))?;
    let fields = match content {
        sui_json_rpc_types::SuiParsedData::MoveObject(obj) => obj.fields.to_json_value(),
        other => bail!("price info field: unexpected content {other:?}"),
    };
    let id = fields
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("price info field has no value: {fields}"))?;
    id.parse().with_context(|| format!("parsing PriceInfoObject id {id:?}"))
}

/// Compose the appraisal with real Pyth legs (when any are needed) and the
/// option-coin bucket map — the full production shape.
async fn compose_with_legs(
    client: &SuiClient,
    http: &reqwest::Client,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &AppraisalRefs,
    holdings: &sui_tx::tx::appraisal::VaultHoldings,
    option_map: &BTreeMap<String, OptionBucketInfo>,
    feeds_by_type: &BTreeMap<String, protocol_types::PriceFeedId>,
) -> Result<sui_types::transaction::Argument> {
    let needed = pyth_assets_needed(holdings, option_map, refs.dbm.as_ref());
    if needed.is_empty() {
        return compose_appraisal(client, pt, refs, holdings, None, option_map).await;
    }
    let handles = pyth_handles();
    let table = resolve_price_info_table(client, handles.pyth_state_id).await?;
    let mut feeds = Vec::new();
    let mut price_infos = BTreeMap::new();
    let mut all: Vec<String> = needed.iter().cloned().collect();
    all.push(holdings.deposit_type.clone());
    for t in &all {
        let Some(feed) = feeds_by_type.get(t) else {
            eprintln!("    (no pyth feed for {t}; passing none leg)");
            continue;
        };
        if !feeds.contains(feed) {
            feeds.push(*feed);
        }
        price_infos.insert(t.clone(), price_info_object_for(client, &table, *feed).await?);
    }
    let (payloads, _) = pyth_client::latest_with_update_data(http, HERMES_BETA, &feeds)
        .await
        .context("fetching hermes update")?;
    let update = payloads.first().ok_or_else(|| anyhow!("hermes returned no payloads"))?;
    compose_appraisal(
        client,
        pt,
        refs,
        holdings,
        Some(PriceLegs { pyth: &handles, accumulator_update: update, price_infos: &price_infos }),
        option_map,
    )
    .await
}

/// Dev-inspect `adapter::custody_balance<T>` for the wrapped manager.
async fn custody_coin_balance(
    client: &SuiClient,
    sender: SuiAddress,
    trading_vault_pkg: ObjectID,
    adapter_pkg: ObjectID,
    vault_id: ObjectID,
    custody_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    use sui_types::transaction::TransactionKind;
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    let custody_arg = pt.pure(custody_id)?;
    let custody_type =
        TypeTag::from_str(&format!("{adapter_pkg}::deepbook_adapter::DeepBookCustody"))?;
    let custody = pt.programmable_move_call(
        trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("borrow_position").unwrap(),
        vec![custody_type],
        vec![vault, custody_arg],
    );
    pt.programmable_move_call(
        adapter_pkg,
        Identifier::new("deepbook_adapter").unwrap(),
        Identifier::new("custody_balance").unwrap(),
        vec![TypeTag::from_str(coin_type)?],
        vec![custody],
    );
    let res = client
        .read_api()
        .dev_inspect_transaction_block(
            sender,
            TransactionKind::ProgrammableTransaction(pt.finish()),
            None,
            None,
            None,
        )
        .await
        .context("dev-inspecting custody_balance")?;
    if let Some(err) = res.error {
        bail!("custody_balance dev-inspect failed: {err}");
    }
    let results = res.results.unwrap_or_default();
    let (bytes, _) = results
        .last()
        .and_then(|r| r.return_values.first())
        .ok_or_else(|| anyhow!("custody_balance returned nothing"))?;
    Ok(u64::from_le_bytes(bytes.as_slice().try_into().context("u64 return")?))
}

/// The vault's `total_shares` (this smoke's wallet is the sole staker).
async fn read_total_shares(client: &SuiClient, vault_id: ObjectID) -> Result<u128> {
    let resp = client
        .read_api()
        .get_object_with_options(vault_id, SuiObjectDataOptions::new().with_content())
        .await?;
    let json = serde_json::to_value(
        resp.data.and_then(|d| d.content).ok_or_else(|| anyhow!("vault unreadable"))?,
    )?;
    let raw = json
        .pointer("/fields/total_shares")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("vault has no total_shares field"))?;
    raw.parse().context("parsing total_shares")
}
