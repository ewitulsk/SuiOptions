//! End-to-end acceptance smoke of the curated trading vault **v2**
//! (`vault_v2`, SO-418) on a live network — the release gate's "SDK
//! behavior checked against the state-transition matrix" artifact
//! (overhaul plan §9.5.3). Exercises vault core (position NFTs, tranches,
//! lanes, settlement pool), the DeepBook adapter, the appraisal composer,
//! and the withdrawal queue against REAL deployed contracts.
//!
//! Default scenario plan:
//!   A. untranched vault: create → deposit (VaultPosition NFT lands in the
//!      wallet) → risk-off read-back (commitment unfunded → risk-off;
//!      funded → risk-on) → curator commitment funding → crank_capital →
//!      split → merge → DeepBook custody leg → request_withdraw
//!      (position-consuming) → fulfill → payout/fee asserts → multi-asset
//!      leg (attested TBTC deposit, global-seq amend, mixed fulfillment)
//!   B. tranched vault (unless --skip-tranched): create senior/junior →
//!      junior seed (commitment + junior deposit) → senior deposit under
//!      the target gate → over-target senior deposit ABORTS 123 →
//!      risk-state read-back → senior withdraw request (lane 0) →
//!      initiate/finalize close → snapshot_settlement →
//!      settle_queued_request + redeem_settled_position +
//!      withdraw_commitment_settled → drained-pool asserts
//!
//! Explicitly NOT smoked here (and why):
//!   - coverage breach / impairment / junior reset / burn_wiped_position:
//!     need real NAV losses and the 7-day reset seasoning — impossible on
//!     a live network without price manipulation. Anchored by the Move
//!     tests named in docs/trading-vault-v2/spec.md §9.
//!   - begin_quote_session's on-chain risk-off abort (124): needs an
//!     adapter witness, i.e. the full exchange stack. Covered INDIRECTLY
//!     by the `vault::is_risk_off` read-back asserts in scenario A (the
//!     same predicate gates the session).
//!   - secondary position transfer: a plain Sui `TransferObjects` on a
//!     `key + store` object — nothing protocol-specific to smoke.
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
use sui_tx::tx::trading_vault as tv;
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
    /// Skip the tranched-vault + terminal-settlement scenarios (v2
    /// acceptance section B). Both are cash-only and need no extra
    /// services; skip only for a quick core-loop iteration.
    #[arg(long, default_value_t = false)]
    skip_tranched: bool,
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

// ═══════════════════ v2 terms provenance (spec.md §preamble) ═══════════════════

/// The spec version every smoke vault binds its terms to.
const TERMS_VERSION: u64 = 1;

/// The exact spec document, embedded at compile time — the smoke runs
/// from the repo checkout, so this always matches what the deployment
/// ceremony recorded (deployment-manager embeds the same file).
const SPEC_MD: &[u8] = include_bytes!("../../../../docs/trading-vault-v2/spec.md");

fn spec_hash() -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(SPEC_MD).to_vec()
}

/// An untranched `CreateVaultSpec`: structure 0, all six tranche params 0
/// (the contract aborts otherwise).
fn untranched_spec() -> tv::CreateVaultSpec {
    tv::CreateVaultSpec {
        lockup_ms: 0,
        curator_fee_bps: 1_000,
        unwind_grace_ms: 3_600_000,
        structure_code: 0,
        senior_hurdle_bps_annual: 0,
        target_junior_bps: 0,
        maintenance_junior_bps: 0,
        upside_code: 0,
        residual_participation_bps: 0,
        total_return_cap_bps: 0,
        terms_version: TERMS_VERSION,
        spec_hash: spec_hash(),
    }
}

/// Senior/junior spec inside the protocol bounds (hurdle ≤ 2000, target ≥
/// 1000, maintenance in [500, target]); PreferredOnly upside.
fn tranched_spec() -> tv::CreateVaultSpec {
    tv::CreateVaultSpec {
        structure_code: 1,
        senior_hurdle_bps_annual: 1_000, // 10% annual
        target_junior_bps: 2_000,        // 20%
        maintenance_junior_bps: 1_000,   // 10%
        upside_code: 0,                  // PreferredOnly (participation/cap must be 0)
        ..untranched_spec()
    }
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
    /// Shared `whitelist::Whitelist` — the ingress gate on writes,
    /// `create_vault`, `vault::deposit` and exchange fills (SO-382/383/384).
    whitelist_id: ObjectID,
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

impl Ids {
    fn appraisal_refs(&self, vault_id: ObjectID) -> AppraisalRefs {
        AppraisalRefs {
            trading_vault_pkg: self.trading_vault_pkg,
            deepbook_adapter_pkg: Some(self.deepbook_adapter_pkg),
            options_adapter_pkg: Some(self.options_adapter_pkg),
            exchange_adapter_pkg: self.exchange_adapter_pkg,
            vault_id,
            protocol_config_id: self.protocol_config_id,
            oracle_registry_id: self.oracle_registry_id,
            // SO-299: smoke vaults have no external account.
            equity_oracle_pkg: None,
            equity_book_id: None,
            vol_book_id: self.vol_book_id,
        }
    }

    fn tv_refs<'a>(&'a self, vault_id: ObjectID) -> tv::TradingVaultRefs<'a> {
        tv::TradingVaultRefs {
            package: self.trading_vault_pkg,
            vault_id,
            protocol_config_id: self.protocol_config_id,
            deposit_type: &self.deposit_coin_type,
        }
    }
}

async fn resolve_ids(client: &ChainClient, cli: &Cli) -> Result<Ids> {
    let deps = deployments::Deployments::load(&cli.deployments)
        .context("loading deployments.json")?;
    let net = deps.for_env(&cli.env)?;
    let pi = &net.package_info;
    let tv_rec = pi.trading_vault.as_ref().ok_or_else(|| anyhow!("no tradingVault record"))?;
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
        let tv_created = created_map(client, &tv_rec.publish_digest).await?;
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
        trading_vault_pkg: tv_rec.package()?,
        oracle_pyth_pkg: op.package()?,
        deepbook_adapter_pkg: dba.package()?,
        options_adapter_pkg: oa.package()?,
        exchange_adapter_pkg: pi.exchange_adapter.as_ref().map(|p| p.package()).transpose()?,
        tokens_pkg: ObjectID::from_hex_literal(&tt.package_id)?,
        protocol_config_id: pc,
        whitelist_id: net.whitelist_object()?,
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
    println!("trading-vault v2 smoke — signer {}, env {}", signer.address, cli.env);

    // Shared context the appraisal legs need.
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

    // ── A1. create the untranched vault (accounting asset TUSDC, creator
    // == curator, no lockup, v2 terms provenance bound to spec.md).
    let step = Step("create_vault (untranched, terms_version 1)");
    let creation = tv::create_vault(
        &client,
        &signer,
        ids.trading_vault_pkg,
        ids.protocol_config_id,
        ids.whitelist_id,
        &ids.deposit_coin_type,
        &untranched_spec(),
        cli.gas_budget,
    )
    .await?;
    let vault_id = creation.vault_id;
    let cap_id = creation.curator_cap_id;
    step.ok();
    println!("    vault {vault_id}");

    let refs = ids.appraisal_refs(vault_id);
    let tv_refs = ids.tv_refs(vault_id);

    // ── A2. faucet-mint + deposit (tranche 0) — the minted VaultPosition
    // NFT must land in this wallet.
    let step = Step("deposit → VaultPosition NFT in wallet");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(&client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(&client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        &client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        cli.deposit_amount,
    )
    .await?;
    tv::build_deposit_and_transfer(
        &client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        appraisal,
        coin,
        /* tranche */ 0,
        signer.address,
    )
    .await?;
    let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit").await?;
    let position_1 = created_position(&resp)?;
    let p1 = position_snapshot(&client, position_1).await?;
    if p1.vault_id != vault_id {
        bail!("position {position_1} names vault {} — expected {vault_id}", p1.vault_id);
    }
    if p1.shares == 0 {
        bail!("minted position {position_1} carries zero shares");
    }
    step.ok();
    println!("    position {position_1} ({} shares, basis {})", p1.shares, p1.basis);

    // ── A3. risk-off read-back: with the curator commitment unfunded the
    // vault is risk-off (commitment breach, §8.6) — the SAME predicate
    // that makes begin_quote_session abort 124 and forces sessions.
    let step = Step("risk-off while commitment unfunded (quote-session gate, indirect)");
    if !vault_is_risk_off(&client, signer.address, &ids, vault_id).await? {
        bail!("vault with zero curator commitment is not risk-off — §8.6 gate broken?");
    }
    step.ok();

    // ── A4. curator commitment funding cures the breach.
    let step = Step("deposit_into_commitment cures the commitment breach");
    let commitment_amount = cli.deposit_amount / 10;
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(&client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(&client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        &client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        commitment_amount,
    )
    .await?;
    tv::build_deposit_into_commitment(
        &client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        cap_id,
        appraisal,
        coin,
    )
    .await?;
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fund_commitment").await?;
    let (exists, commitment_shares) =
        commitment_of(&client, signer.address, &ids, vault_id, cap_id).await?;
    if !exists || commitment_shares == 0 {
        bail!("commitment escrow missing after deposit_into_commitment");
    }
    if vault_is_risk_off(&client, signer.address, &ids, vault_id).await? {
        bail!("vault still risk-off after funding the commitment");
    }
    step.ok();
    println!("    commitment {commitment_shares} shares escrowed");

    // ── A5. permissionless capital crank (the keeper's cadence call).
    let step = Step("crank_capital");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(&client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(&client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    tv::build_crank_capital(&client, &mut pt, &tv_refs, appraisal).await?;
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::crank_capital").await?;
    step.ok();

    // ── A6. split conserves shares+basis; merge restores them.
    let step = Step("split + merge conserve shares and basis");
    let half = p1.shares / 2;
    let mut pt = ProgrammableTransactionBuilder::new();
    let child = tv::build_split_position(&client, &mut pt, &tv_refs, position_1, half).await?;
    pt.transfer_arg(signer.address, child);
    let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::split").await?;
    let child_id = created_position(&resp)?;
    let (parent_after, child_after) = (
        position_snapshot(&client, position_1).await?,
        position_snapshot(&client, child_id).await?,
    );
    if child_after.shares != half || parent_after.shares != p1.shares - half {
        bail!(
            "split shares wrong: parent {} + child {} != {}",
            parent_after.shares,
            child_after.shares,
            p1.shares
        );
    }
    if parent_after.basis + child_after.basis != p1.basis {
        bail!(
            "split lost basis: {} + {} != {}",
            parent_after.basis,
            child_after.basis,
            p1.basis
        );
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_merge_positions(&client, &mut pt, &tv_refs, position_1, child_id).await?;
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::merge").await?;
    let merged = position_snapshot(&client, position_1).await?;
    if merged.shares != p1.shares || merged.basis != p1.basis {
        bail!("merge did not restore shares/basis: {} / {}", merged.shares, merged.basis);
    }
    step.ok();

    if cli.direct_escrow {
        run_direct_escrow_leg(&client, &signer, &cli, &ids, net_top, live.as_ref(), vault_id, cap_id)
            .await?;
        return Ok(());
    }

    let mut custody: Option<(ObjectID, ObjectID, String, String)> = None;
    let mut position_2: Option<ObjectID> = None;
    let deposit_tag = TypeTag::from_str(&ids.deposit_coin_type)?;
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
                let inner = inner.strip_suffix('>').unwrap_or(inner);
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
            let minted =
                faucet_mint(&client, &mut pt, ids.tokens_pkg, u_faucet, &u_module, write_amt)
                    .await?;
            let bucket = pt.obj(shared_object_arg(&client, bref.bucket_id, true).await?)?;
            let wl = pt
                .obj(shared_object_arg(&client, ids.whitelist_id, false).await?)?;
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
                vec![bucket, wl, minted, clock],
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

        // ── 4. deposit again WITH the custody live (appraisal covers it);
        // second VaultPosition NFT to this wallet.
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
        let coin = faucet_mint(
            &client,
            &mut pt,
            ids.tokens_pkg,
            ids.deposit_faucet,
            &ids.deposit_module,
            cli.deposit_amount,
        )
        .await?;
        tv::build_deposit_and_transfer(
            &client,
            &mut pt,
            &tv_refs,
            ids.whitelist_id,
            appraisal,
            coin,
            /* tranche */ 0,
            signer.address,
        )
        .await?;
        let resp =
            submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit_with_custody").await?;
        position_2 = Some(created_position(&resp)?);
        step.ok();
    }

    if cli.fill_bid {
        // ── 5'. fill-bid mode: crystallize a PARTIAL withdrawal through the
        // option-coin appraisal — v2 partials consume a whole position, so
        // the first deposit's position exits while the second stays — then
        // leave the vault OPEN with its CALL inventory (the live MM-vault
        // state). Full-drain coverage lives in the default (no-fill) mode.
        let step = Step("partial withdraw + fulfill (option-coin appraisal)");
        let before = read_total_shares(&client, vault_id).await?;
        let exiting = position_snapshot(&client, position_1).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        tv::build_request_withdraw(&client, &mut pt, &tv_refs, &ids.deposit_coin_type, position_1)
            .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw_partial").await?;

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
        tv::build_fulfill_withdrawals(&client, &mut pt, &tv_refs, ids.treasury_id, appraisal)
            .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fulfill_partial").await?;
        if pending_withdrawals(&client, signer.address, &ids, vault_id).await? != 0 {
            bail!("partial withdrawal not fulfilled");
        }
        // The filled bid usually leaves the exiting lot with a mark
        // profit, whose performance fee mints shares into the curator
        // commitment (§5) — so the burn is NET of a small fee mint.
        let after = read_total_shares(&client, vault_id).await?;
        let net_burn = before.saturating_sub(after);
        if net_burn < exiting.shares * 9 / 10 {
            bail!(
                "share supply only dropped {net_burn} of the {} requested — fulfillment paid \
                 the wrong lot?",
                exiting.shares
            );
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

    // ── 6. exit everything: merge the second position into the first,
    // request-withdraw the merged position (consumed whole), fulfill. The
    // depositor's claim is exactly the two deposits (no profit → no fee).
    let step = Step("merge + request_withdraw (position-consuming) + fulfill");
    if let Some(p2) = position_2 {
        let mut pt = ProgrammableTransactionBuilder::new();
        tv::build_merge_positions(&client, &mut pt, &tv_refs, position_1, p2).await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::merge_deposits").await?;
    }
    let exiting = position_snapshot(&client, position_1).await?;
    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_request_withdraw(&client, &mut pt, &tv_refs, &ids.deposit_coin_type, position_1)
        .await?;
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw").await?;
    if pending_withdrawals(&client, signer.address, &ids, vault_id).await? != 1 {
        bail!("queue should hold exactly the one consumed-position request");
    }

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
    tv::build_fulfill_withdrawals(&client, &mut pt, &tv_refs, ids.treasury_id, appraisal).await?;
    submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::fulfill").await?;
    step.ok();
    println!("    exited {} shares", exiting.shares);

    // ── 7. verify payouts/fees: queue drained; the user's shares are gone
    // (only the escrowed commitment remains — v2 keeps it in the book);
    // with no profit, no fee — the vault's accounting balance is exactly
    // the commitment's value (± floor dust).
    let step = Step("verify payouts (queue drained, only the commitment remains)");
    if pending_withdrawals(&client, signer.address, &ids, vault_id).await? != 0 {
        bail!("withdrawal queue not drained");
    }
    let (senior, junior) = read_book_shares(&client, vault_id).await?;
    if senior != 0 || junior != commitment_shares {
        bail!(
            "book ({senior} senior / {junior} junior) != commitment-only ({commitment_shares})"
        );
    }
    let holdings = discover_holdings(&client, vault_id).await?;
    let residual = !holdings.free_assets.is_empty()
        || holdings.positions.iter().any(|p| {
            !matches!(
                p,
                sui_tx::tx::appraisal::PositionInfo::DeepBookCustody { assets, pools, .. }
                    if assets.is_empty() && pools.is_empty()
            )
        });
    if residual {
        bail!("vault still holds non-accounting assets/positions after full exit: {holdings:?}");
    }
    let vault_bal = vault_free_balance(
        &client,
        signer.address,
        ids.trading_vault_pkg,
        vault_id,
        &ids.deposit_coin_type,
    )
    .await?;
    if vault_bal.abs_diff(commitment_amount) > 8 {
        bail!(
            "vault accounting balance {vault_bal} != commitment {commitment_amount} — \
             payout or fee math off"
        );
    }
    step.ok();

    // ── 8. multi-asset leg (SO-370): curator allowlists TBTC, a depositor
    // deposits it with the attestation-bearing composer (minting a TBTC-lot
    // position), requests payout in TUSDC then amends (by GLOBAL sequence)
    // to TBTC, and the fulfillment potato pays it out.
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
        let coin =
            faucet_mint(&client, &mut pt, ids.tokens_pkg, tbtc_faucet, "tbtc", cli.deposit_amount)
                .await?;
        let minted = tv::build_deposit_asset(
            &client,
            &mut pt,
            &tv_refs,
            ids.whitelist_id,
            &tbtc_type,
            appraisal,
            coin,
            tbtc_att,
            /* tranche */ 0,
        )
        .await?;
        pt.transfer_arg(signer.address, minted);
        let resp = submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::deposit_tbtc").await?;
        let tbtc_position = created_position(&resp)?;
        let deposited =
            vault_free_balance(&client, signer.address, ids.trading_vault_pkg, vault_id, &tbtc_type)
                .await?;
        if deposited != cli.deposit_amount {
            bail!("vault TBTC balance {deposited} != deposited {}", cli.deposit_amount);
        }
        step.ok();

        // Request payout in the accounting asset, then exercise the
        // recipient-only amend (keyed by GLOBAL sequence) over to TBTC.
        let step = Step("multi-asset: request (TUSDC) + amend payout to TBTC by global_seq");
        let seq = read_next_global_seq(&client, vault_id).await?;
        let mut pt = ProgrammableTransactionBuilder::new();
        tv::build_request_withdraw(
            &client,
            &mut pt,
            &tv_refs,
            &ids.deposit_coin_type,
            tbtc_position,
        )
        .await?;
        submit_ptb(&client, &signer, pt, cli.gas_budget, "smoke::request_withdraw_tbtc").await?;
        if read_next_global_seq(&client, vault_id).await? != seq + 1 {
            bail!("global sequence did not advance by the request");
        }
        let mut pt = ProgrammableTransactionBuilder::new();
        tv::build_amend_payout_asset(&client, &mut pt, &tv_refs, &tbtc_type, seq).await?;
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
        tv::build_fulfill_mixed(
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

        // Queue drained and (almost) all TBTC paid out. The escrowed
        // commitment keeps its pro-rata sliver of the TBTC value; only
        // that plus floor dust may remain.
        if pending_withdrawals(&client, signer.address, &ids, vault_id).await? != 0 {
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

    // ── B. tranched vault + terminal settlement acceptance (cash-only —
    // no extra services needed).
    if !cli.skip_tranched {
        run_tranched_and_settlement(&client, &signer, &cli, &ids).await?;
    }

    println!("\nSMOKE PASSED — vault {vault_id} exercised end to end (v2)");
    Ok(())
}

// ═══════════════ tranched + settlement scenarios (v2, section B) ═══════════════

/// Senior/junior vault against the state-transition matrix, then the
/// terminal settlement pool (§8.7): junior must seed before senior; an
/// over-target senior deposit aborts 123; a queued senior request and the
/// wallet/escrow positions all settle against the frozen pool.
async fn run_tranched_and_settlement(
    client: &ChainClient,
    signer: &Signer,
    cli: &Cli,
    ids: &Ids,
) -> Result<()> {
    let junior_seed = cli.deposit_amount; // commitment (junior for tranched)
    let junior_user = cli.deposit_amount; // wallet junior deposit
    let senior_ok = cli.deposit_amount * 4; // inside the 20% target gate
    let senior_over = cli.deposit_amount * 50; // would breach the gate

    // ── B1. create.
    let step = Step("tranched: create_vault (senior/junior, PreferredOnly)");
    let creation = tv::create_vault(
        client,
        signer,
        ids.trading_vault_pkg,
        ids.protocol_config_id,
        ids.whitelist_id,
        &ids.deposit_coin_type,
        &tranched_spec(),
        cli.gas_budget,
    )
    .await?;
    let vault_id = creation.vault_id;
    let cap_id = creation.curator_cap_id;
    let refs = ids.appraisal_refs(vault_id);
    let tv_refs = ids.tv_refs(vault_id);
    step.ok();
    println!("    vault {vault_id}");

    // ── B2. junior seed: curator commitment (junior tranche for a
    // tranched vault) + a wallet junior deposit.
    let step = Step("tranched: junior seed (commitment + junior deposit)");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        junior_seed,
    )
    .await?;
    tv::build_deposit_into_commitment(
        client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        cap_id,
        appraisal,
        coin,
    )
    .await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::tranched_commitment").await?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        junior_user,
    )
    .await?;
    tv::build_deposit_and_transfer(
        client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        appraisal,
        coin,
        /* tranche */ 2,
        signer.address,
    )
    .await?;
    let resp = submit_ptb(client, signer, pt, cli.gas_budget, "smoke::junior_deposit").await?;
    let junior_position = created_position(&resp)?;
    let jp = position_snapshot(client, junior_position).await?;
    if jp.tranche != "Junior" || jp.generation != 0 {
        bail!("junior position minted as {} gen {}", jp.tranche, jp.generation);
    }
    step.ok();

    // ── B3. senior deposit inside the target gate.
    let step = Step("tranched: senior deposit under the target gate");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        senior_ok,
    )
    .await?;
    tv::build_deposit_and_transfer(
        client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        appraisal,
        coin,
        /* tranche */ 1,
        signer.address,
    )
    .await?;
    let resp = submit_ptb(client, signer, pt, cli.gas_budget, "smoke::senior_deposit").await?;
    let senior_position = created_position(&resp)?;
    let sp = position_snapshot(client, senior_position).await?;
    if sp.tranche != "Senior" {
        bail!("senior position minted as {}", sp.tranche);
    }
    step.ok();

    // ── B4. an over-target senior deposit must abort 123
    // (senior_buffer_breached): junior 2e6 of a would-be 56e6 total is
    // far below the 20% target.
    let step = Step("tranched: over-target senior deposit aborts 123");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    let coin = faucet_mint(
        client,
        &mut pt,
        ids.tokens_pkg,
        ids.deposit_faucet,
        &ids.deposit_module,
        senior_over,
    )
    .await?;
    tv::build_deposit_and_transfer(
        client,
        &mut pt,
        &tv_refs,
        ids.whitelist_id,
        appraisal,
        coin,
        /* tranche */ 1,
        signer.address,
    )
    .await?;
    match submit_ptb(client, signer, pt, cli.gas_budget, "smoke::senior_over_target").await {
        Ok(_) => bail!("over-target senior deposit was ACCEPTED — target gate broken"),
        Err(e) => assert_move_abort(&e, 123, "over-target senior deposit")?,
    }
    step.ok();

    // ── B5. risk-state read-back.
    let step = Step("tranched: risk-state + book read-back");
    let (senior_shares, junior_shares) = read_book_shares(client, vault_id).await?;
    if senior_shares == 0 || junior_shares == 0 {
        bail!("book missing a tranche: {senior_shares} senior / {junior_shares} junior");
    }
    let state = read_risk_state(client, vault_id).await?;
    if state != "Healthy" {
        bail!("expected Healthy risk state, read {state}");
    }
    let claim = read_book_u128(client, vault_id, "senior_claim").await?;
    // The claim is the senior principal plus minutes of 10%-annual accrual.
    if claim < senior_ok as u128 || claim > (senior_ok as u128) * 101 / 100 {
        bail!("senior claim {claim} not ≈ principal {senior_ok}");
    }
    if vault_is_risk_off(client, signer.address, ids, vault_id).await? {
        bail!("healthy funded tranched vault reads risk-off");
    }
    step.ok();
    println!("    book: {senior_shares}s/{junior_shares}j shares, claim {claim}");

    // ── B6. queue a senior withdrawal (lane 0), then close the vault with
    // the request still outstanding — it must settle from the pool.
    let step = Step("settlement: queue senior request, initiate+finalize close");
    let queued_seq = read_next_global_seq(client, vault_id).await?;
    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_request_withdraw(client, &mut pt, &tv_refs, &ids.deposit_coin_type, senior_position)
        .await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::senior_request").await?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let cap = pt.obj(sui_tx::tx::owned_object_arg(client, cap_id).await?)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("initiate_close").unwrap(),
        vec![],
        vec![vault_arg, cap],
    );
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("finalize_close").unwrap(),
        vec![],
        vec![vault_arg],
    );
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::close").await?;
    step.ok();

    // ── B7. snapshot freezes entitlements (senior first).
    let step = Step("settlement: snapshot_settlement");
    let mut pt = ProgrammableTransactionBuilder::new();
    let holdings = discover_holdings(client, vault_id).await?;
    let (appraisal, _) =
        compose_appraisal(client, &mut pt, &refs, &holdings, None, &BTreeMap::new(), &[]).await?;
    tv::build_snapshot_settlement(client, &mut pt, &tv_refs, appraisal).await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::snapshot_settlement").await?;
    step.ok();

    // ── B8. drain the pool: the queued senior request, the wallet junior
    // position, and the escrowed commitment (withdraw + redeem).
    let step = Step("settlement: settle queued request + redeem positions");
    let before = vault_free_balance(
        client,
        signer.address,
        ids.trading_vault_pkg,
        vault_id,
        &ids.deposit_coin_type,
    )
    .await?;

    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_settle_queued_request(client, &mut pt, &tv_refs, ids.treasury_id, queued_seq).await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::settle_queued").await?;
    if pending_withdrawals(client, signer.address, ids, vault_id).await? != 0 {
        bail!("queued request still outstanding after settle_queued_request");
    }

    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_redeem_settled_position(client, &mut pt, &tv_refs, ids.treasury_id, junior_position)
        .await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::redeem_junior").await?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let commitment_pos =
        tv::build_withdraw_commitment_settled(client, &mut pt, &tv_refs, cap_id).await?;
    pt.transfer_arg(signer.address, commitment_pos);
    let resp =
        submit_ptb(client, signer, pt, cli.gas_budget, "smoke::withdraw_commitment").await?;
    let commitment_position = created_position(&resp)?;
    let mut pt = ProgrammableTransactionBuilder::new();
    tv::build_redeem_settled_position(
        client,
        &mut pt,
        &tv_refs,
        ids.treasury_id,
        commitment_position,
    )
    .await?;
    submit_ptb(client, signer, pt, cli.gas_budget, "smoke::redeem_commitment").await?;

    // Fees crystallize per redemption; with ~zero profit over the smoke's
    // runtime, the curator's accrued settlement fee is normally 0 — claim
    // it only when nonzero (claiming zero aborts).
    let fees_accrued = read_settlement_fees(client, vault_id).await.unwrap_or(0);
    if fees_accrued > 0 {
        let mut pt = ProgrammableTransactionBuilder::new();
        tv::build_claim_settlement_curator_fees(client, &mut pt, &tv_refs, cap_id).await?;
        submit_ptb(client, signer, pt, cli.gas_budget, "smoke::claim_settlement_fees").await?;
        println!("    claimed {fees_accrued} settlement curator fees");
    }

    let after = vault_free_balance(
        client,
        signer.address,
        ids.trading_vault_pkg,
        vault_id,
        &ids.deposit_coin_type,
    )
    .await?;
    if after >= before {
        bail!("settlement paid nothing out ({before} → {after})");
    }
    // senior_pool + junior_pool == NAV exactly; the three redemptions
    // leave only per-redemption floor dust.
    if after > 1_000 {
        bail!("vault kept {after} accounting units after full settlement drain");
    }
    step.ok();
    println!(
        "\nTRANCHED + SETTLEMENT PASSED — vault {vault_id} closed, settled, drained \
         (residual {after})"
    );
    Ok(())
}

// ═══════════════════════ shared PTB / read helpers ═══════════════════════

/// Faucet-mint `amount` of a test token inside `pt`, returning the Coin
/// argument.
async fn faucet_mint(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    tokens_pkg: ObjectID,
    faucet_id: ObjectID,
    module: &str,
    amount: u64,
) -> Result<sui_types::transaction::Argument> {
    let faucet = pt.obj(shared_object_arg(client, faucet_id, true).await?)?;
    let amount = pt.pure(amount)?;
    Ok(pt.programmable_move_call(
        tokens_pkg,
        Identifier::new(module).context("faucet module name")?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount],
    ))
}

/// The freshly minted `vault_position::VaultPosition` in a tx's created
/// objects.
fn created_position(resp: &sui_tx::chain::ExecutedTransaction) -> Result<ObjectID> {
    created_objects(resp)
        .into_iter()
        .find_map(|c| {
            let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
            (tag.module.as_str() == "vault_position" && tag.name.as_str() == "VaultPosition")
                .then_some(c.object_id)
        })
        .ok_or_else(|| anyhow!("no VaultPosition created by the transaction"))
}

/// gRPC json renders Move enums as `{"@variant": "Healthy"}` (bare string
/// kept as a fallback) — see the api-service goldens.
fn enum_variant(v: &serde_json::Value) -> Option<&str> {
    v.as_str().or_else(|| v.pointer("/@variant").and_then(|x| x.as_str()))
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64())
}

fn json_u128(v: &serde_json::Value) -> Option<u128> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64().map(u128::from))
}

struct PositionSnapshot {
    vault_id: ObjectID,
    shares: u128,
    basis: u64,
    tranche: String,
    generation: u64,
}

async fn position_snapshot(client: &ChainClient, id: ObjectID) -> Result<PositionSnapshot> {
    let (_, json) = client.get_object_json(id).await?;
    let json = json.ok_or_else(|| anyhow!("position {id} unreadable"))?;
    let field = |ptr: &str| {
        json.pointer(ptr).ok_or_else(|| anyhow!("position {id} has no {ptr}"))
    };
    Ok(PositionSnapshot {
        vault_id: field("/vault_id")?
            .as_str()
            .ok_or_else(|| anyhow!("position {id} vault_id not a string"))?
            .parse()?,
        shares: json_u128(field("/shares")?)
            .ok_or_else(|| anyhow!("position {id} shares unparseable"))?,
        basis: json_u64(field("/cost_basis")?)
            .ok_or_else(|| anyhow!("position {id} cost_basis unparseable"))?,
        tranche: enum_variant(field("/tranche")?)
            .ok_or_else(|| anyhow!("position {id} tranche unreadable"))?
            .to_string(),
        generation: json_u64(field("/capital_generation")?)
            .ok_or_else(|| anyhow!("position {id} capital_generation unparseable"))?,
    })
}

async fn vault_json(client: &ChainClient, vault_id: ObjectID) -> Result<serde_json::Value> {
    let (_, json) = client.get_object_json(vault_id).await?;
    json.ok_or_else(|| anyhow!("vault {vault_id} unreadable"))
}

/// The v2 book's per-tranche supplies (an untranched vault keeps its whole
/// supply in `junior_shares`).
async fn read_book_shares(client: &ChainClient, vault_id: ObjectID) -> Result<(u128, u128)> {
    let json = vault_json(client, vault_id).await?;
    let read = |field: &str| {
        json.pointer(&format!("/book/{field}"))
            .and_then(json_u128)
            .ok_or_else(|| anyhow!("vault has no readable book.{field}"))
    };
    Ok((read("senior_shares")?, read("junior_shares")?))
}

async fn read_book_u128(client: &ChainClient, vault_id: ObjectID, field: &str) -> Result<u128> {
    let json = vault_json(client, vault_id).await?;
    json.pointer(&format!("/book/{field}"))
        .and_then(json_u128)
        .ok_or_else(|| anyhow!("vault has no readable book.{field}"))
}

async fn read_risk_state(client: &ChainClient, vault_id: ObjectID) -> Result<String> {
    let json = vault_json(client, vault_id).await?;
    json.pointer("/book/risk_state")
        .and_then(enum_variant)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("vault has no readable book.risk_state"))
}

async fn read_next_global_seq(client: &ChainClient, vault_id: ObjectID) -> Result<u64> {
    let json = vault_json(client, vault_id).await?;
    json.pointer("/next_global_seq")
        .and_then(json_u64)
        .ok_or_else(|| anyhow!("vault has no readable next_global_seq"))
}

/// `settlement.curator_fees_accrued` once the snapshot exists.
async fn read_settlement_fees(client: &ChainClient, vault_id: ObjectID) -> Result<u64> {
    let json = vault_json(client, vault_id).await?;
    json.pointer("/settlement/curator_fees_accrued")
        .and_then(json_u64)
        .ok_or_else(|| anyhow!("vault has no readable settlement.curator_fees_accrued"))
}

/// The vault's total share supply — v2 has no single field; sum the book.
async fn read_total_shares(client: &ChainClient, vault_id: ObjectID) -> Result<u128> {
    let (senior, junior) = read_book_shares(client, vault_id).await?;
    Ok(senior + junior)
}

/// Dev-inspect `vault::is_risk_off` — the predicate gating quote sessions
/// (abort 124), external release, and vault_mm release.
async fn vault_is_risk_off(
    client: &ChainClient,
    sender: SuiAddress,
    ids: &Ids,
    vault_id: ObjectID,
) -> Result<bool> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("is_risk_off").unwrap(),
        vec![],
        vec![vault],
    );
    let res = client.dev_inspect_ptb(sender, pt).await.context("dev-inspecting is_risk_off")?;
    sui_tx::chain::decode_return_value::<bool>(&res, 0).context("decoding is_risk_off")
}

/// Dev-inspect `vault::pending_withdrawals` — outstanding queued requests.
async fn pending_withdrawals(
    client: &ChainClient,
    sender: SuiAddress,
    ids: &Ids,
    vault_id: ObjectID,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("pending_withdrawals").unwrap(),
        vec![],
        vec![vault],
    );
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting pending_withdrawals")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding pending_withdrawals")
}

/// Dev-inspect `vault::commitment_of(vault, cap_id)` → (exists, shares).
async fn commitment_of(
    client: &ChainClient,
    sender: SuiAddress,
    ids: &Ids,
    vault_id: ObjectID,
    cap_id: ObjectID,
) -> Result<(bool, u128)> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    let cap = pt.pure(cap_id)?;
    pt.programmable_move_call(
        ids.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("commitment_of").unwrap(),
        vec![],
        vec![vault, cap],
    );
    let res = client.dev_inspect_ptb(sender, pt).await.context("dev-inspecting commitment_of")?;
    let exists = sui_tx::chain::decode_return_value::<bool>(&res, 0)
        .context("decoding commitment_of.exists")?;
    let shares = sui_tx::chain::decode_return_value::<u128>(&res, 1)
        .context("decoding commitment_of.shares")?;
    Ok((exists, shares))
}

/// Assert `err` is the Move abort `code` (v2 wire codes are part of the
/// frozen interface — see contracts/trading-vault-v2/sources/errors.move).
fn assert_move_abort(err: &anyhow::Error, code: u64, what: &str) -> Result<()> {
    let s = format!("{err:#}");
    if s.contains("MoveAbort") && s.contains(&format!(", {code})")) {
        return Ok(());
    }
    bail!("{what}: expected Move abort {code}, got: {s}")
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

// ═══════════════════ direct vault escrow leg (SO-372) ═══════════════════

/// Curator wires the vault for direct quoting (identity BM custody +
/// quote-adapter opt-in + delegated signer), then this wallet taker-fills a
/// signed maker order through `exchange_adapter::fill_vault_order_reverse`
/// (the vault sells its accounting-asset free balance for a faucet-minted
/// base). Verifies value moved vault↔taker with the identity BM holding
/// nothing, and leaves the vault open.
///
/// v2 ordering note: this leg runs AFTER the commitment is funded — the
/// quote path is risk-off-gated (§8.4b), so an unfunded commitment would
/// abort the fill with 124.
#[allow(clippy::too_many_arguments)]
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
    let taker_coin =
        faucet_mint(client, &mut pt, ids.tokens_pkg, base_faucet, &base_module, buy_amount)
            .await?;
    let vault_arg = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
    let vreg = pt.obj(shared_object_arg(client, ids.integration_registry_id, false).await?)?;
    let reg = pt.obj(shared_object_arg(client, market.registry()?, true).await?)?;
    // Ingress whitelist (SO-384): required by every fill entry.
    let wl = pt.obj(shared_object_arg(client, ids.whitelist_id, false).await?)?;
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
            vault_arg, vreg, reg, wl, bm, custody_arg, order_bytes, sig, pk, taker_coin,
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
    // The vault holds deposit + commitment in quote before the fill.
    if quote_after >= cli.deposit_amount + cli.deposit_amount / 10 {
        bail!("vault quote balance {quote_after} did not decrease");
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
    println!("    vault sold quote for {base_after} base units");
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
