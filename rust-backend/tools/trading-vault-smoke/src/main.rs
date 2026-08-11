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
use sui_tx::chain::{created_objects, ChainClient};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::SuiKeyPair;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use sui_tx::sui_client::Signer;
use sui_tx::tx::appraisal::{
    compose_appraisal, discover_holdings, price_assets_needed, AppraisalRefs, OptionBucketInfo,
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
    /// Skip the multi-asset leg (SO-370: TBTC allowlist + attested
    /// deposit + amended payout + fulfillment potato).
    #[arg(long, default_value_t = false)]
    skip_multi_asset: bool,
    /// Stop after create/deposit/custody-fund and leave the vault live
    /// (for pointing a vault-mode mm-bot at it). Prints the ids to keep.
    #[arg(long, default_value_t = false)]
    keep_open: bool,
    /// Direct-escrow leg (SO-372), replacing the DeepBook/drain legs:
    /// curator init_direct_custody + add_quote_adapter + add_signer, a
    /// maker order signed with the delegated key against the identity BM,
    /// taker-filled through exchange_adapter::fill_vault_order_reverse;
    /// asserts vault balances moved and the identity BM held nothing.
    /// Leaves the vault open. Opt-in (unlike --skip-*) because it needs
    /// the SO-372 exchange + exchange_adapter contracts deployed.
    #[arg(long, default_value_t = false)]
    direct_escrow: bool,
    /// Fill the vault's resting bid with self-written calls (SO-297): the
    /// custody ends up holding a nonzero option-coin balance, the deposit
    /// and a partial fulfillment run through `options_oracle` legs, and
    /// the vault is left OPEN (the live MM-vault state) instead of drained.
    #[arg(long, default_value_t = false)]
    fill_bid: bool,
    /// oracle-service base URL (SO-346). When set, price legs follow the
    /// live `/oracle/descriptor` provider — required to smoke a
    /// Switchboard-flipped deployment. Absent, the compiled Pyth path
    /// runs as before.
    #[arg(long)]
    oracle_url: Option<String>,
}

/// The live oracle switch, resolved once from `--oracle-url` (SO-346).
struct LiveOracle {
    client: oracle_client::OracleClient,
    descriptor: oracle_client::OracleDescriptor,
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
    exchange_adapter_pkg: Option<ObjectID>,
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

async fn resolve_ids(client: &ChainClient, cli: &Cli) -> Result<Ids> {
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
        exchange_adapter_pkg: pi.exchange_adapter.as_ref().map(|p| p.package()).transpose()?,
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

async fn created_map(client: &ChainClient, digest: &str) -> Result<BTreeMap<String, ObjectID>> {
    let resp = client.get_transaction(&digest.parse()?).await?;
    let ids: Vec<ObjectID> = created_objects(&resp).iter().map(|c| c.object_id).collect();
    let objs = client.multi_get_objects(&ids).await?;
    let mut out = BTreeMap::new();
    for o in objs {
        if let Some(t) = o.struct_tag() {
            out.insert(format!("{}::{}", t.module, t.name), o.id());
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
    let client = ChainClient::new(&cli.rpc)?;
    let signer = load_signer(&cli.address)?;
    let ids = resolve_ids(&client, &cli).await?;
    let live = match &cli.oracle_url {
        Some(url) => {
            let oc = oracle_client::OracleClient::new(url);
            let descriptor = oc.descriptor().await.context("fetching /oracle/descriptor")?;
            println!(
                "  live oracle provider: {} (adapter {})",
                descriptor.provider,
                if descriptor.adapter.is_some() { "deployed" } else { "MISSING" },
            );
            Some(LiveOracle { client: oc, descriptor })
        }
        None => None,
    };
    println!("trading-vault smoke — signer {}, env {}", signer.address, cli.env);

    // ── 1. create vault (deposit asset TUSDC, creator == curator, no lockup)
    let step = Step("create_vault");
    let mut pt = ProgrammableTransactionBuilder::new();
    let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
    let a0 = pt.pure(0u64)?; // lockup
    let a1 = pt.pure(1_000u64)?; // curator fee bps
    let a2 = pt.pure(3_600_000u64)?; // unwind grace
    let deposit_tag = TypeTag::from_str(&ids.deposit_coin_type)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("create_vault").unwrap(),
        vec![deposit_tag.clone()],
        vec![cfg, a0, a1, a2],
    );
    let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::create_vault").await?;
    let (mut vault_id, mut cap_id) = (None, None);
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        match tag.name.as_str() {
            "TradingVault" => vault_id = Some(c.object_id),
            "CuratorCap" => cap_id = Some(c.object_id),
            _ => {}
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
        deepbook_adapter_pkg: Some(ids.deepbook_adapter_pkg),
        options_adapter_pkg: Some(ids.options_adapter_pkg),
        exchange_adapter_pkg: ids.exchange_adapter_pkg,
        vault_id,
        protocol_config_id: ids.protocol_config_id,
        oracle_registry_id: ids.oracle_registry_id,
        // SO-299: the smoke vault has no external account.
        equity_oracle_pkg: None,
        equity_book_id: None,
        vol_book_id: ids.vol_book_id,
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
    let (appraisal, _) =
        compose_appraisal(&client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
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
    let att = opt_attestation(&mut pt, ids.trading_vault_pkg, None)?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![deposit_tag.clone()],
        vec![vault_arg, cfg, appraisal, coin, att, clock],
    );
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit").await?;
    step.ok();

    if cli.direct_escrow {
        run_direct_escrow_leg(&client, &signer, &cli, &ids, net_top, live.as_ref(), vault_id, cap_id)
            .await?;
        return Ok(());
    }

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
        let custody_id = created_objects(&resp)
            .into_iter()
            .find_map(|c| {
                let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
                (tag.name.as_str() == "DeepBookCustody").then_some(c.object_id)
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
        let (_, allow_json) = client.get_object_json(ids.pool_allowlist_id).await?;
        let allow_json = allow_json.ok_or_else(|| anyhow!("allowlist unreadable"))?;
        let pool_ids: Vec<String> = allow_json
            .pointer("/allowed/contents")
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
                .get_object(pid)
                .await?
                .struct_tag()
                .map(|t| t.to_canonical_string(/* with_prefix */ true))
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
        let (appraisal, _) = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
            ids.oracle_pyth_pkg,
            ids.pyth_feed_registry_id,
            live.as_ref(),
            signer.address,
            cli.gas_budget,
            &[],
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
        let att = opt_attestation(&mut pt, ids.trading_vault_pkg, None)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cfg, appraisal, coin, att, clock],
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
            vec![deposit_tag.clone()],
            vec![vault_arg, shares, clock],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw_half").await?;

        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, _) = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
            ids.oracle_pyth_pkg,
            ids.pyth_feed_registry_id,
            live.as_ref(),
            signer.address,
            cli.gas_budget,
            &[],
        )
        .await?;
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
        let treasury = pt.obj(shared_object_arg(&client, ids.treasury_id, true).await?)?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("fulfill_withdrawals").unwrap(),
            vec![deposit_tag.clone()],
            vec![vault_arg, cfg, treasury, appraisal, clock],
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
        vec![deposit_tag.clone()],
        vec![vault_arg, shares, clock],
    );
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw").await?;

    let holdings = discover_holdings(&client, vault_id).await?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let (appraisal, _) = compose_with_legs(
        &client,
        &http,
        &mut pt,
        &refs,
        &holdings,
        &option_map,
        &feeds_by_type,
        ids.oracle_pyth_pkg,
        ids.pyth_feed_registry_id,
        live.as_ref(),
        signer.address,
        cli.gas_budget,
        &[],
    )
    .await?;
    let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(&client, ids.treasury_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("fulfill_withdrawals").unwrap(),
        vec![deposit_tag.clone()],
        vec![vault_arg, cfg, treasury, appraisal, clock],
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

    // ── 8. multi-asset leg (SO-370): curator allowlists TBTC, a depositor
    // deposits it with the attestation-bearing composer, requests payout in
    // TUSDC then amends to TBTC, and the fulfillment potato pays it out.
    if !cli.skip_multi_asset {
        let step = Step("multi-asset: allowlist TBTC + attested deposit");
        let tt = net_top
            .package_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens record"))?;
        let tbtc = tt.tokens.get("TBTC").ok_or_else(|| anyhow!("no TBTC test token"))?;
        let tbtc_type = protocol_types::asset::canonicalize_move_type(&tbtc.coin_type);
        let tbtc_tag = TypeTag::from_str(&tbtc_type)?;
        let tbtc_faucet = tbtc.faucet()?;
        let tv_refs = sui_tx::tx::trading_vault::TradingVaultRefs {
            package: ids.trading_vault_pkg,
            vault_id,
            protocol_config_id: ids.protocol_config_id,
            deposit_type: &ids.deposit_coin_type,
        };

        // Curator allowlists TBTC for deposits and payout requests.
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cap = pt.obj(sui_tx::tx::owned_object_arg(&client, cap_id).await?)?;
        let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("add_deposit_asset").unwrap(),
            vec![tbtc_tag.clone()],
            vec![vault_arg, cap, cfg],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::add_deposit_asset").await?;

        // Attested TBTC deposit: the deposit's `option::some` reuses the
        // SAME attest result the composer emitted (attestations are copy).
        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, attestations) = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
            ids.oracle_pyth_pkg,
            ids.pyth_feed_registry_id,
            live.as_ref(),
            signer.address,
            cli.gas_budget,
            std::slice::from_ref(&tbtc_type),
        )
        .await?;
        let tbtc_att = *attestations
            .get(&tbtc_type)
            .ok_or_else(|| anyhow!("no TBTC attestation composed — feed missing?"))?;
        let faucet = pt.obj(shared_object_arg(&client, tbtc_faucet, true).await?)?;
        let amount = pt.pure(cli.deposit_amount)?;
        let coin = pt.programmable_move_call(
            ids.tokens_pkg,
            Identifier::new("tbtc").unwrap(),
            Identifier::new("mint").unwrap(),
            vec![],
            vec![faucet, amount],
        );
        let vault_arg = pt.obj(shared_object_arg(&client, vault_id, true).await?)?;
        let cfg = pt.obj(shared_object_arg(&client, ids.protocol_config_id, false).await?)?;
        let att = opt_attestation(&mut pt, ids.trading_vault_pkg, Some(tbtc_att))?;
        let clock = clock_arg(&mut pt)?;
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![tbtc_tag.clone()],
            vec![vault_arg, cfg, appraisal, coin, att, clock],
        );
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit_tbtc").await?;
        let deposited =
            vault_free_balance(&client, signer.address, ids.trading_vault_pkg, vault_id, &tbtc_type)
                .await?;
        if deposited != cli.deposit_amount {
            bail!("vault TBTC balance {deposited} != deposited {}", cli.deposit_amount);
        }
        step.ok();

        // Request payout in the accounting asset, then exercise the
        // recipient-only amend over to TBTC.
        let step = Step("multi-asset: request (TUSDC) + amend payout to TBTC");
        let shares = read_total_shares(&client, vault_id).await?;
        if shares == 0 {
            bail!("no shares after the TBTC deposit");
        }
        let mut pt = ProgrammableTransactionBuilder::new();
        sui_tx::tx::trading_vault::build_request_withdraw(
            &client,
            &mut pt,
            &tv_refs,
            &ids.deposit_coin_type,
            shares,
        )
        .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw_tbtc").await?;
        let seq = {
            let (_, json) = client.get_object_json(vault_id).await?;
            let json = json.ok_or_else(|| anyhow!("vault unreadable"))?;
            json.pointer("/queue_tail")
                .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
                .ok_or_else(|| anyhow!("vault has no queue_tail field"))?
                .checked_sub(1)
                .ok_or_else(|| anyhow!("queue_tail is 0 after a request"))?
        };
        let mut pt = ProgrammableTransactionBuilder::new();
        sui_tx::tx::trading_vault::build_amend_payout_asset(
            &client,
            &mut pt,
            &tv_refs,
            &tbtc_type,
            seq,
        )
        .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::amend_payout_asset").await?;
        step.ok();

        // The fulfillment potato: the appraisal's TBTC attest (mandatory —
        // it's now a free balance) doubles as the batch price.
        let step = Step("multi-asset: fulfillment potato pays TBTC");
        let holdings = discover_holdings(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, attestations) = compose_with_legs(
            &client,
            &http,
            &mut pt,
            &refs,
            &holdings,
            &option_map,
            &feeds_by_type,
            ids.oracle_pyth_pkg,
            ids.pyth_feed_registry_id,
            live.as_ref(),
            signer.address,
            cli.gas_budget,
            &[],
        )
        .await?;
        let tbtc_att = *attestations
            .get(&tbtc_type)
            .ok_or_else(|| anyhow!("no TBTC attestation composed for the fulfillment"))?;
        sui_tx::tx::trading_vault::build_fulfill_mixed(
            &client,
            &mut pt,
            &tv_refs,
            ids.treasury_id,
            appraisal,
            vec![tbtc_att],
            &[(tbtc_type.clone(), 1)],
        )
        .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fulfill_mixed").await?;

        // Queue drained and (almost) all TBTC paid out: only floor-division
        // dust — bounded by price drift between deposit and fulfillment —
        // may remain in the vault's free balance.
        let (_, json) = client.get_object_json(vault_id).await?;
        let json = json.ok_or_else(|| anyhow!("vault unreadable"))?;
        let q = |ptr: &str| {
            json.pointer(ptr)
                .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
        };
        if q("/queue_head") != q("/queue_tail") {
            bail!("withdrawal queue not drained by the fulfillment potato");
        }
        let residual =
            vault_free_balance(&client, signer.address, ids.trading_vault_pkg, vault_id, &tbtc_type)
                .await?;
        if residual * 50 > cli.deposit_amount {
            bail!(
                "vault kept {residual} TBTC of {} deposited — payout did not happen?",
                cli.deposit_amount
            );
        }
        step.ok();
        println!("    TBTC round-tripped (residual dust {residual})");
    }

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
        price_info_table_id: None,
    }
}

const HERMES_BETA: &str = "https://hermes-beta.pyth.network";

/// Pyth's feed → `PriceInfoObject` table (port of keeper::discovery).
struct PriceInfoTable {
    table_id: ObjectID,
    identifier_type: move_core_types::language_storage::TypeTag,
}

async fn resolve_price_info_table(client: &ChainClient, pyth_state_id: ObjectID) -> Result<PriceInfoTable> {
    // Derive the field id client-side — some providers don't serve a
    // dynamic-field index (same approach as keeper/src/discovery.rs).
    let key_bytes = bcs::to_bytes(b"price_info".to_vec().as_slice())
        .context("bcs of the price_info field name")?;
    let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
        pyth_state_id,
        &TypeTag::from_str("vector<u8>").expect("static type tag"),
        &key_bytes,
    )
    .context("deriving pyth price_info field id")?;
    let (_, field_json) = client
        .get_object_json(field_id)
        .await
        .context("reading pyth state price_info dynamic field")?;
    let table_id: ObjectID = field_json
        .as_ref()
        .and_then(|j| j.pointer("/value/id").or_else(|| j.pointer("/value")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("pyth state {pyth_state_id} has no price_info table"))?
        .parse()
        .context("parsing price_info table id")?;
    let type_str = client
        .get_object(table_id)
        .await?
        .struct_tag()
        .map(|t| t.to_canonical_string(/* with_prefix */ true))
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
    client: &ChainClient,
    table: &PriceInfoTable,
    feed: protocol_types::PriceFeedId,
) -> Result<ObjectID> {
    let key_bytes = bcs::to_bytes(&feed.0.to_vec()).context("bcs of feed id")?;
    let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
        table.table_id,
        &table.identifier_type,
        &key_bytes,
    )
    .context("deriving price info field id")?;
    let fields = client
        .try_get_object_json(field_id)
        .await
        .with_context(|| format!("looking up price info object for feed {feed}"))?
        .and_then(|(_, json)| json)
        .ok_or_else(|| anyhow!("feed {feed} has no PriceInfoObject on this network"))?;
    let id = fields
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("price info field has no value: {fields}"))?;
    id.parse().with_context(|| format!("parsing PriceInfoObject id {id:?}"))
}

/// `Option<PriceAttestation>` for `vault::deposit`'s att slot: `some`
/// wrapping a composed attest result (non-accounting deposits, SO-370),
/// `none` for the accounting asset.
fn opt_attestation(
    pt: &mut ProgrammableTransactionBuilder,
    trading_vault_pkg: ObjectID,
    att: Option<sui_types::transaction::Argument>,
) -> Result<sui_types::transaction::Argument> {
    let attestation_type =
        TypeTag::from_str(&format!("{trading_vault_pkg}::price::PriceAttestation"))?;
    let (function, args) = match att {
        Some(a) => ("some", vec![a]),
        None => ("none", vec![]),
    };
    Ok(pt.programmable_move_call(
        ObjectID::from_hex_literal("0x1").unwrap(),
        Identifier::new("option").unwrap(),
        Identifier::new(function).unwrap(),
        vec![attestation_type],
        args,
    ))
}

/// Compose the appraisal with real Pyth legs (when any are needed) and the
/// option-coin bucket map — the full production shape. `extras` are
/// assets to attest beyond what the holdings need (SO-370: a
/// non-accounting deposit's own asset); returns the appraisal plus the
/// per-asset attestations.
#[allow(clippy::too_many_arguments)]
async fn compose_with_legs(
    client: &ChainClient,
    http: &reqwest::Client,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &AppraisalRefs,
    holdings: &sui_tx::tx::appraisal::VaultHoldings,
    option_map: &BTreeMap<String, OptionBucketInfo>,
    feeds_by_type: &BTreeMap<String, protocol_types::PriceFeedId>,
    // The adapter identity moved onto the legs (SO-335) so a provider
    // switch cannot leave one caller pairing Pyth's registry with a
    // different adapter's `attest`.
    oracle_pyth_pkg: ObjectID,
    pyth_feed_registry_id: ObjectID,
    live: Option<&LiveOracle>,
    // The Pyth update fee is funded to match how `sender` pays gas.
    sender: sui_types::base_types::SuiAddress,
    gas_budget: u64,
    extras: &[String],
) -> Result<(
    sui_types::transaction::Argument,
    BTreeMap<String, sui_types::transaction::Argument>,
)> {
    let needed = price_assets_needed(holdings, option_map);
    if needed.is_empty() && extras.is_empty() {
        return compose_appraisal(client, pt, refs, holdings, None, option_map, &[]).await;
    }
    // SO-346: with a live descriptor saying Switchboard, build that
    // provider's legs (shared composer, SO-375); otherwise (no
    // --oracle-url, or provider=pyth) the compiled Pyth path below runs
    // unchanged.
    if let Some(l) = live {
        if l.descriptor.provider == protocol_types::OracleProvider::Switchboard {
            return sui_tx::tx::appraisal::compose_switchboard_appraisal(
                client,
                pt,
                refs,
                holdings,
                option_map,
                &l.descriptor,
                &l.client,
                extras,
            )
            .await;
        }
    }
    let handles = pyth_handles();
    let table = resolve_price_info_table(client, handles.pyth_state_id).await?;
    let mut feeds = Vec::new();
    let mut price_infos: BTreeMap<String, ObjectID> = BTreeMap::new();
    let mut all: Vec<String> = needed.iter().cloned().collect();
    all.extend(extras.iter().cloned());
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
        Some(sui_tx::tx::oracle::OracleLegs::Pyth(sui_tx::tx::oracle::PythLegs {
            adapter_pkg: oracle_pyth_pkg,
            feed_registry_id: pyth_feed_registry_id,
            handles: &handles,
            accumulator_update: update,
            price_infos: &price_infos,
            sender,
            gas_budget,
        })),
        option_map,
        extras,
    )
    .await
}

/// Dev-inspect `adapter::custody_balance<T>` for the wrapped manager.
async fn custody_coin_balance(
    client: &ChainClient,
    sender: SuiAddress,
    trading_vault_pkg: ObjectID,
    adapter_pkg: ObjectID,
    vault_id: ObjectID,
    custody_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
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
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting custody_balance")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding custody_balance")
}

/// Dev-inspect `vault::free_balance_of<T>` — the vault's free balance in
/// `coin_type` smallest units (0 when the balance df was pruned).
async fn vault_free_balance(
    client: &ChainClient,
    sender: SuiAddress,
    trading_vault_pkg: ObjectID,
    vault_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    pt.programmable_move_call(
        trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("free_balance_of").unwrap(),
        vec![TypeTag::from_str(coin_type)?],
        vec![vault],
    );
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting free_balance_of")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding free_balance_of")
}

/// The vault's `total_shares` (this smoke's wallet is the sole staker).
async fn read_total_shares(client: &ChainClient, vault_id: ObjectID) -> Result<u128> {
    let (_, json) = client.get_object_json(vault_id).await?;
    let json = json.ok_or_else(|| anyhow!("vault unreadable"))?;
    let raw = json
        .pointer("/total_shares")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("vault has no total_shares field"))?;
    raw.parse().context("parsing total_shares")
}

// ═══════════════════ direct vault escrow leg (SO-372) ═══════════════════

/// Curator wires the vault for direct quoting (identity BM custody +
/// quote-adapter opt-in + delegated signer), then this wallet taker-fills a
/// signed maker order through `exchange_adapter::fill_vault_order_reverse`
/// (the vault sells its accounting-asset free balance for a faucet-minted
/// base). Verifies value moved vault↔taker with the identity BM holding
/// nothing, and leaves the vault open.
async fn run_direct_escrow_leg(
    client: &ChainClient,
    signer: &Signer,
    cli: &Cli,
    ids: &Ids,
    net: &deployments::NetworkDeployment,
    live: Option<&LiveOracle>,
    vault_id: ObjectID,
    cap_id: ObjectID,
) -> Result<()> {
    let canon = protocol_types::asset::canonicalize_move_type;
    let pi = &net.package_info;
    let ea_pkg = pi
        .exchange_adapter
        .as_ref()
        .ok_or_else(|| anyhow!("no exchangeAdapter record — is SO-372 deployed?"))?
        .package()?;
    let ex = net.exchange()?;
    let ex_pkg = ex.package()?;
    let tt = pi.test_tokens.as_ref().ok_or_else(|| anyhow!("no testTokens record"))?;

    // Pick a market quoted in the vault's accounting asset whose base is a
    // faucet-mintable test token (the taker's payment).
    let mut picked = None;
    for (sym, m) in &ex.markets {
        if canon(&m.quote) != ids.deposit_coin_type {
            continue;
        }
        let base = canon(&m.base);
        if let Some((bsym, tok)) =
            tt.tokens.iter().find(|(_, t)| canon(&t.coin_type) == base)
        {
            picked = Some((sym.clone(), m.clone(), base, bsym.to_lowercase(), tok.faucet()?));
            break;
        }
    }
    let (sym, market, base_type, base_module, base_faucet) = picked.ok_or_else(|| {
        anyhow!(
            "no exchange market quoted in {} with a faucet-mintable base",
            ids.deposit_coin_type
        )
    })?;
    let base_tag = TypeTag::from_str(&base_type)?;
    let quote_tag = TypeTag::from_str(&ids.deposit_coin_type)?;
    println!("    market {sym} (registry {})", market.registry_id);

    // ── curator: direct custody + quote-adapter opt-in, one PTB.
    let step = Step("direct escrow: init_direct_custody + add_quote_adapter");
    let witness_tag =
        TypeTag::from_str(&format!("{ea_pkg}::exchange_adapter::ExchangeAdapter"))?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let cap = pt.obj(sui_tx::tx::owned_object_arg(client, cap_id).await?)?;
    let ireg = pt.obj(shared_object_arg(client, ids.integration_registry_id, false).await?)?;
    pt.programmable_move_call(
        ea_pkg,
        Identifier::new("exchange_adapter").unwrap(),
        Identifier::new("init_direct_custody").unwrap(),
        vec![],
        vec![vault_arg, cap, ireg],
    );
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("add_quote_adapter").unwrap(),
        vec![witness_tag],
        vec![vault_arg, cap],
    );
    let resp =
        submit_ptb(client, signer, pt, cli.gas_budget, "smoke::init_direct_custody").await?;
    let (mut custody_id, mut bm_id) = (None, None);
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        match tag.name.as_str() {
            "ExchangeCustody" => custody_id = Some(c.object_id),
            "BalanceManager" => bm_id = Some(c.object_id),
            _ => {}
        }
    }
    let custody_id = custody_id.ok_or_else(|| anyhow!("no ExchangeCustody created"))?;
    let bm_id = bm_id.ok_or_else(|| anyhow!("no identity BalanceManager created"))?;
    client.await_object(bm_id, 6).await.context("waiting for the identity BM")?;
    step.ok();
    println!("    custody {custody_id}\n    identity BM {bm_id}");

    // ── curator: delegate this wallet as the order-signing hot key.
    let step = Step("direct escrow: add_signer (delegate this wallet)");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let cap = pt.obj(sui_tx::tx::owned_object_arg(client, cap_id).await?)?;
    let ireg = pt.obj(shared_object_arg(client, ids.integration_registry_id, false).await?)?;
    let bm = pt.obj(shared_object_arg(client, bm_id, true).await?)?;
    let custody_arg = pt.pure(custody_id)?;
    let delegate = pt.pure(signer.address)?;
    pt.programmable_move_call(
        ea_pkg,
        Identifier::new("exchange_adapter").unwrap(),
        Identifier::new("add_signer").unwrap(),
        vec![],
        vec![vault_arg, cap, ireg, bm, custody_arg, delegate],
    );
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::add_signer").await?;
    step.ok();

    // ── curator: allowlist every catalog test token for deposits (SO-375).
    // The staging-mm-bot funding pass deposits faucet-minted base inventory
    // into this vault, so each `{SYM}/TUSDC` market's base must be
    // depositable. Depositability is oracle-coverage self-gating at deposit
    // time; warn early when the live descriptor has no feed.
    let step = Step("direct escrow: allowlist catalog tokens as deposit assets");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let cap = pt.obj(sui_tx::tx::owned_object_arg(client, cap_id).await?)?;
    let cfg = pt.obj(shared_object_arg(client, ids.protocol_config_id, false).await?)?;
    let mut listed = Vec::new();
    for (bsym, tok) in &tt.tokens {
        let t = canon(&tok.coin_type);
        if t == ids.deposit_coin_type {
            continue;
        }
        if let Some(l) = live {
            if l.descriptor.provider == protocol_types::OracleProvider::Switchboard
                && !l.descriptor.feeds.contains_key(&t)
            {
                println!(
                    "    WARNING: no live feed for {bsym} ({t}) — deposits will fail until one is seeded"
                );
            }
        }
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("add_deposit_asset").unwrap(),
            vec![TypeTag::from_str(&t)?],
            vec![vault_arg, cap, cfg],
        );
        listed.push(bsym.clone());
    }
    if listed.is_empty() {
        println!("    (no non-accounting test tokens to allowlist)");
    } else {
        submit_ptb(client, signer, pt, cli.gas_budget, "smoke::add_deposit_assets").await?;
        println!("    allowlisted: {}", listed.join(", "));
    }
    step.ok();

    // ── maker order: the vault sells Quote (its accounting asset) for
    // Base, signed with the delegated key against the identity BM.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64;
    let sell_amount = cli.deposit_amount / 2; // quote out of the vault
    let buy_amount = cli.deposit_amount / 2; // base the taker pays in full
    let registry = exchange_types::SuiAddress::parse(&market.registry_id)
        .map_err(|e| anyhow!("registry hex: {e}"))?;
    let ex_addr = |id: &ObjectID| {
        exchange_types::SuiAddress::parse(&id.to_string())
            .map_err(|e| anyhow!("address hex: {e}"))
    };
    let order = exchange_types::Order {
        maker_token: ids.deposit_coin_type.clone(),
        taker_token: base_type.clone(),
        maker_amount: sell_amount,
        taker_amount: buy_amount,
        max_fee_bps: 50,
        maker: ex_addr(&vault_id)?,
        maker_manager_id: ex_addr(&bm_id)?,
        taker: exchange_types::SuiAddress::ZERO,
        sender: exchange_types::SuiAddress::ZERO,
        expiry_ms: now_ms + 3_600_000,
        salt: now_ms,
    };
    let kp = order_keypair(&signer.keypair)?;
    let digest = exchange_signing::order_digest(&order, &registry);
    let signed = exchange_types::order::SignedOrder {
        signature: kp.sign_personal_message(&digest.0),
        public_key: kp.public_key(),
        scheme: exchange_types::order::SignatureScheme::Ed25519,
        order: order.clone(),
        registry_id: registry,
    };

    // ── taker fill: mint the base payment and settle through the adapter.
    let step = Step("direct escrow: taker fill via fill_vault_order_reverse");
    let mut pt = ProgrammableTransactionBuilder::new();
    let faucet = pt.obj(shared_object_arg(client, base_faucet, true).await?)?;
    let amount = pt.pure(buy_amount)?;
    let taker_coin = pt.programmable_move_call(
        ids.tokens_pkg,
        Identifier::new(&*base_module).unwrap(),
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount],
    );
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let vreg = pt.obj(shared_object_arg(client, ids.integration_registry_id, false).await?)?;
    let reg = pt.obj(shared_object_arg(client, market.registry()?, true).await?)?;
    let bm = pt.obj(shared_object_arg(client, bm_id, false).await?)?;
    let custody_arg = pt.pure(custody_id)?;
    let order_bytes = pt.pure(order.to_bcs())?;
    let sig = pt.pure(signed.prefixed_signature())?;
    let pk = pt.pure(signed.public_key.clone())?;
    let fill_amount = pt.pure(buy_amount)?;
    let min_out = pt.pure(0u64)?;
    let clock = clock_arg(&mut pt)?;
    let out = pt.programmable_move_call(
        ea_pkg,
        Identifier::new("exchange_adapter").unwrap(),
        Identifier::new("fill_vault_order_reverse").unwrap(),
        vec![base_tag, quote_tag],
        vec![
            vault_arg, vreg, reg, bm, custody_arg, order_bytes, sig, pk, taker_coin,
            fill_amount, min_out, clock,
        ],
    );
    let sui_types::transaction::Argument::Result(i) = out else {
        bail!("unexpected fill_vault_order_reverse result shape");
    };
    pt.transfer_arg(signer.address, sui_types::transaction::Argument::NestedResult(i, 0));
    pt.transfer_arg(signer.address, sui_types::transaction::Argument::NestedResult(i, 1));
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::fill_vault_order_reverse").await?;
    step.ok();

    // ── verify: quote left the vault, base arrived, the identity BM is a
    // pure pass-through (held nothing).
    let step = Step("direct escrow: vault balances moved, identity BM empty");
    let quote_after = vault_free_balance(
        client,
        signer.address,
        ids.trading_vault_pkg,
        vault_id,
        &ids.deposit_coin_type,
    )
    .await?;
    let base_after =
        vault_free_balance(client, signer.address, ids.trading_vault_pkg, vault_id, &base_type)
            .await?;
    if quote_after >= cli.deposit_amount {
        bail!("vault quote balance {quote_after} did not decrease from {}", cli.deposit_amount);
    }
    if base_after == 0 {
        bail!("vault gained no {base_type} from the fill");
    }
    for t in [&ids.deposit_coin_type, &base_type] {
        let held = bm_balance_of(client, signer.address, ex_pkg, bm_id, t).await?;
        if held != 0 {
            bail!("identity BM holds {held} of {t} — direct escrow leaked into the manager");
        }
    }
    step.ok();
    println!(
        "    vault sold {} quote units, gained {base_after} base units",
        cli.deposit_amount - quote_after
    );
    println!("\nDIRECT-ESCROW SMOKE PASSED — vault {vault_id} left open with direct quoting on");
    Ok(())
}

/// Dev-inspect `balance_manager::balance_of<T>` on the exchange package.
async fn bm_balance_of(
    client: &ChainClient,
    sender: SuiAddress,
    exchange_pkg: ObjectID,
    bm_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(client, bm_id, false).await?)?;
    pt.programmable_move_call(
        exchange_pkg,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("balance_of").unwrap(),
        vec![TypeTag::from_str(coin_type)?],
        vec![bm],
    );
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting balance_of")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding balance_of")
}

/// The exchange-signing keypair for this wallet's ed25519 key — the smoke
/// signs maker orders with the same key that pays gas (delegated to the
/// identity BM via `add_signer`). Same flag‖seed extraction as the
/// staging-mm-bot's OrderSigner.
fn order_keypair(kp: &SuiKeyPair) -> Result<exchange_signing::keys::Ed25519Keypair> {
    use base64::Engine;
    use sui_types::crypto::EncodeDecodeBase64;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(kp.encode_base64())
        .context("decoding keypair bytes")?;
    if bytes.len() != 33 || bytes[0] != 0x00 {
        bail!("--direct-escrow requires an ed25519 signer key");
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes[1..33]);
    Ok(exchange_signing::keys::Ed25519Keypair::from_seed(seed))
}
