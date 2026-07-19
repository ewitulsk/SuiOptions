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
use sui_tx::tx::appraisal::{compose_appraisal, discover_holdings, AppraisalRefs};
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
    tokens_pkg: ObjectID,
    protocol_config_id: ObjectID,
    integration_registry_id: ObjectID,
    oracle_registry_id: ObjectID,
    pyth_feed_registry_id: ObjectID,
    pool_allowlist_id: ObjectID,
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
        tokens_pkg: ObjectID::from_hex_literal(&tt.package_id)?,
        protocol_config_id: pc,
        integration_registry_id: ireg,
        oracle_registry_id: oreg,
        pyth_feed_registry_id: freg,
        pool_allowlist_id: plist,
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

/// pool id → the bucket's underlying coin type, from the indexer (the
/// pool's BASE is the per-roll call coin, which inherits the
/// underlying's decimals — needed to re-derive the tick/lot grid).
async fn fetch_pool_underlyings(url: &str) -> Result<BTreeMap<String, String>> {
    let body = serde_json::json!({
        "query": "{ buckets { assetType deepbookPoolId } }"
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let mut out = BTreeMap::new();
    for b in resp
        .pointer("/data/buckets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let (Some(asset), Some(pool)) = (
            b.pointer("/assetType").and_then(|v| v.as_str()),
            b.pointer("/deepbookPoolId").and_then(|v| v.as_str()),
        ) {
            out.insert(
                canon_id(pool),
                protocol_types::asset::canonicalize_move_type(asset),
            );
        }
    }
    Ok(out)
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
        options_adapter_pkg: None,
        vault_id,
        protocol_config_id: ids.protocol_config_id,
        oracle_registry_id: ids.oracle_registry_id,
        pyth_feed_registry_id: ids.pyth_feed_registry_id,
    };
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(&client, vault_id).await?;
    let appraisal = compose_appraisal(&client, &mut pt, &refs, &holdings, None).await?;
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
        let pool_underlyings = match fetch_pool_underlyings(&cli.indexer_graphql).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("    (indexer pool lookup failed: {e:#})");
                BTreeMap::new()
            }
        };
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
                let Some(underlying) = pool_underlyings.get(&canon_id(pid_str)) else {
                    eprintln!("    skip {pid_str}: not in indexer map");
                    continue;
                };
                let Some(base_decimals) = decimals_of(underlying) else {
                    eprintln!("    skip {pid_str}: no decimals for {underlying}");
                    continue;
                };
                picked = Some((pid, parts[0].to_string(), parts[1].to_string(), base_decimals));
                break;
            }
        }
        let (pool_id, base_ty, quote_ty, base_decimals) = picked.ok_or_else(|| {
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
        custody = Some((custody_id, pool_id, base_ty, quote_ty));

        // ── 4. deposit again WITH the custody live (appraisal covers it)
        let step = Step("deposit with live custody (composed appraisal)");
        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let appraisal = compose_appraisal(&client, &mut pt, &refs, &holdings, None).await?;
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
    let appraisal = compose_appraisal(&client, &mut pt, &refs, &holdings, None).await?;
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
