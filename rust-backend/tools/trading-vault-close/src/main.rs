//! Close stale curated trading vaults on a live network (SO-361; v2
//! semantics since SO-418).
//!
//! `trading-vault-smoke --fill_bid` leaves its $1 vault open by design, and
//! interrupted runs strand more — each holding one DeepBookCustody position
//! that blocks `finalize_close`. This tool enumerates open vaults this wallet
//! created from the indexer, verifies each against the chain, and drives the
//! full unwind in ONE PTB per vault:
//!
//!   initiate_close → cancel_all_orders + withdraw_settled (if a pool is
//!   tracked) → force_sweep<T> per tracked asset (prunes zero-balance tags;
//!   force sessions unlock in Closing) → retire_pool → eject_empty_custody →
//!   finalize_close
//!
//! v2: a Closed vault is not done until it is SETTLED (§8.7) — after
//! `finalize_close` the tool triggers the one-time `snapshot_settlement`
//! (cash-only appraisal; closure already drained everything but the
//! accounting asset) and then drains any outstanding queued withdraw
//! requests via the permissionless `settle_queued_request`. Wallet-held
//! `VaultPosition` NFTs redeem against the frozen pool at any later time
//! (`redeem_settled_position`) — this tool does not chase them. Vaults
//! already Closed but unsettled are picked up and settled too.
//!
//! Safety rails: only vaults where this wallet is creator AND current curator,
//! whose every position is a DeepBookCustody (the live desk vault holds a
//! VaultMm position, so it can never match), and whose PER-TRANCHE share
//! supply (book senior AND junior — v2 has no single total_shares) is at
//! most --max-shares (default $10 raw). Dry-run by default; pass --execute
//! to submit.
//!
//!   cargo run -p trading-vault-close -- --address 0xab8d… [--execute]

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_tx::chain::ChainClient;
use sui_tx::sui_client::Signer;
use sui_tx::tx::{clock_arg, owned_object_arg, shared_object_arg, submit_ptb};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::SuiKeyPair;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

#[derive(Parser)]
struct Cli {
    /// Signer address; must be the vaults' creator and curator, with its key
    /// in the local sui keystore.
    #[arg(long)]
    address: String,
    /// gRPC fullnode endpoint (JSON-RPC hosts will not work).
    #[arg(long, default_value = "https://fullnode.testnet.sui.io:443")]
    rpc: String,
    #[arg(long, default_value = "staging")]
    env: String,
    #[arg(long, default_value = "deployments.json")]
    deployments: PathBuf,
    #[arg(long, default_value = "https://sui-options.com/staging/indexer/graphql")]
    indexer_graphql: String,
    #[arg(long, default_value_t = 100_000_000)]
    gas_budget: u64,
    /// Refuse to close a vault with more shares than this (raw units of the
    /// deposit asset) — keeps the live desk vault untouchable even if every
    /// other rail fails.
    #[arg(long, default_value_t = 10_000_000)]
    max_shares: u128,
    /// Submit the transactions. Without this flag the tool only prints what
    /// it would do.
    #[arg(long, default_value_t = false)]
    execute: bool,
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
    deepbook_adapter_pkg: ObjectID,
    options_adapter_pkg: Option<ObjectID>,
    exchange_adapter_pkg: Option<ObjectID>,
    integration_registry_id: ObjectID,
    // v2 settlement (SO-418): snapshot consumes an appraisal and the
    // queued-request drain pays through the treasury.
    protocol_config_id: ObjectID,
    oracle_registry_id: ObjectID,
    vol_book_id: Option<ObjectID>,
    equity_oracle_pkg: Option<ObjectID>,
    equity_book_id: Option<ObjectID>,
    treasury_id: ObjectID,
}

fn resolve_ids(cli: &Cli) -> Result<Ids> {
    let deps = deployments::Deployments::load(&cli.deployments)
        .context("loading deployments.json")?;
    let net = deps.for_env(&cli.env)?;
    let pi = &net.package_info;
    let tv = pi.trading_vault.as_ref().ok_or_else(|| anyhow!("no tradingVault record"))?;
    let dba = pi
        .deepbook_adapter
        .as_ref()
        .ok_or_else(|| anyhow!("no deepbookAdapter record"))?;
    let objs = pi
        .trading_vault_objects
        .as_ref()
        .ok_or_else(|| anyhow!("no tradingVaultObjects record"))?;
    Ok(Ids {
        trading_vault_pkg: tv.package()?,
        deepbook_adapter_pkg: dba.package()?,
        options_adapter_pkg: pi.options_adapter.as_ref().map(|p| p.package()).transpose()?,
        exchange_adapter_pkg: pi.exchange_adapter.as_ref().map(|p| p.package()).transpose()?,
        integration_registry_id: objs.integration_registry()?,
        protocol_config_id: objs.vault_protocol_config()?,
        oracle_registry_id: objs.oracle_registry()?,
        vol_book_id: objs.vol_book()?,
        equity_oracle_pkg: pi.equity_oracle.as_ref().map(|p| p.package()).transpose()?,
        equity_book_id: objs.equity_book()?,
        treasury_id: net.treasury()?,
    })
}

/// One open vault from the indexer, with its single custody position.
struct StaleVault {
    vault_id: ObjectID,
    cap_id: ObjectID,
    /// `None` when the vault has no position left (already ejected).
    custody_id: Option<ObjectID>,
}

async fn gql(url: &str, query: &str, vars: serde_json::Value) -> Result<serde_json::Value> {
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "query": query, "variables": vars }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(errs) = resp.get("errors") {
        bail!("indexer error: {errs}");
    }
    Ok(resp)
}

async fn enumerate_stale(cli: &Cli, me: SuiAddress) -> Result<Vec<StaleVault>> {
    let me_hex = me.to_string();
    // v2: no single totalShares — the per-tranche share rail is enforced
    // against the on-chain book in `verify_on_chain`, so the indexer is
    // only used for enumeration here.
    let resp = gql(
        &cli.indexer_graphql,
        "{ tradingVaults { vaultId state creator curator curatorCapId } }",
        serde_json::json!({}),
    )
    .await?;
    let vaults = resp
        .pointer("/data/tradingVaults")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for v in vaults {
        let get = |k: &str| v.pointer(&format!("/{k}")).and_then(|x| x.as_str());
        // v2: "closed" vaults are still in scope until settled (§8.7);
        // "closing" covers interrupted earlier runs.
        if !matches!(get("state"), Some("open" | "closing" | "closed")) {
            continue;
        }
        let (Some(vid), Some(creator), Some(curator), Some(cap)) =
            (get("vaultId"), get("creator"), get("curator"), get("curatorCapId"))
        else {
            continue;
        };
        if creator != me_hex || curator != me_hex {
            continue;
        }
        let ps = gql(
            &cli.indexer_graphql,
            "query($v:String!){ tradingVaultPositions(vaultId:$v){ positionId adapter active } }",
            serde_json::json!({ "v": vid }),
        )
        .await?;
        let active: Vec<&serde_json::Value> = ps
            .pointer("/data/tradingVaultPositions")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter(|p| p.pointer("/active").and_then(|b| b.as_bool()) == Some(true))
                    .collect()
            })
            .unwrap_or_default();
        if active.len() > 1 {
            println!("  skip {vid}: {} positions (expected at most 1)", active.len());
            continue;
        }
        let custody_id = match active.first() {
            Some(p) => {
                let adapter = p.pointer("/adapter").and_then(|a| a.as_str()).unwrap_or("");
                if !adapter.ends_with("::deepbook_adapter::DeepBookAdapter") {
                    println!("  skip {vid}: position adapter {adapter} is not DeepBookAdapter");
                    continue;
                }
                let pid = p.pointer("/positionId").and_then(|a| a.as_str()).unwrap();
                Some(ObjectID::from_hex_literal(pid)?)
            }
            None => None,
        };
        out.push(StaleVault {
            vault_id: ObjectID::from_hex_literal(vid)?,
            cap_id: ObjectID::from_hex_literal(cap)?,
            custody_id,
        });
    }
    Ok(out)
}

/// On-chain custody snapshot: tracked asset types (canonical, `0x`-prefixed)
/// and the tracked pool with its base/quote generics.
struct CustodySnapshot {
    assets: Vec<String>,
    pool: Option<(ObjectID, String, String)>,
}

/// gRPC json renders Move enums as `{"@variant": "Open"}` (bare string kept
/// as a fallback) — see the api-service goldens.
fn enum_variant(v: &serde_json::Value) -> Option<&str> {
    v.as_str().or_else(|| v.pointer("/@variant").and_then(|x| x.as_str()))
}

struct Plan {
    state: String,
    custody: Option<CustodySnapshot>,
    senior_shares: u128,
    junior_shares: u128,
    settled: bool,
}

async fn verify_on_chain(
    client: &ChainClient,
    ids: &Ids,
    me: SuiAddress,
    max_shares: u128,
    v: &StaleVault,
) -> Result<Option<Plan>> {
    let (_, vault_json) = client.get_object_json(v.vault_id).await?;
    let vault_json = vault_json.ok_or_else(|| anyhow!("vault {} unreadable", v.vault_id))?;
    let state = vault_json
        .pointer("/state")
        .and_then(enum_variant)
        .ok_or_else(|| anyhow!("vault {} has no readable state", v.vault_id))?
        .to_string();
    let settled =
        dev_inspect_bool(client, me, ids.trading_vault_pkg, v.vault_id, "is_settled", &[]).await?;
    if state == "Closed" && settled {
        // v2 done-state: Closed AND snapshot taken. Any queued requests
        // are drained by `settle_vault` right after the snapshot, and
        // wallet-held positions redeem permissionlessly at any time —
        // nothing left for this tool.
        return Ok(None);
    }
    if vault_json.pointer("/creator").and_then(|x| x.as_str()) != Some(&me.to_string()) {
        bail!("vault {} on-chain creator is not the signer", v.vault_id);
    }
    // v2 per-tranche share rail (there is no single total_shares): both
    // book supplies must be under --max-shares. An untranched vault keeps
    // its whole supply in `junior_shares`.
    let book_shares = |field: &str| -> Result<u128> {
        vault_json
            .pointer(&format!("/book/{field}"))
            .and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_u64().map(u128::from)))
            .ok_or_else(|| anyhow!("vault {} has no readable book.{field}", v.vault_id))
    };
    let senior_shares = book_shares("senior_shares")?;
    let junior_shares = book_shares("junior_shares")?;
    if senior_shares > max_shares || junior_shares > max_shares {
        bail!(
            "vault {} book ({senior_shares} senior / {junior_shares} junior shares) exceeds \
             --max-shares (the live desk vault?)",
            v.vault_id
        );
    }

    // The cap must be a CuratorCap owned by the signer.
    let cap = client.get_object(v.cap_id).await?;
    let tag = cap.struct_tag().ok_or_else(|| anyhow!("cap {} has no type", v.cap_id))?;
    if tag.name.as_str() != "CuratorCap" {
        bail!("object {} is a {}, not a CuratorCap", v.cap_id, tag.name);
    }

    let custody = match v.custody_id {
        None => None,
        Some(cid) => {
            let obj = client.get_object(cid).await?;
            let tag = obj.struct_tag().ok_or_else(|| anyhow!("custody {cid} has no type"))?;
            if tag.name.as_str() != "DeepBookCustody" {
                bail!("position {cid} is a {}, not a DeepBookCustody", tag.name);
            }
            let (_, json) = client.get_object_json(cid).await?;
            let json = json.ok_or_else(|| anyhow!("custody {cid} unreadable"))?;
            let assets: Vec<String> = json
                .pointer("/assets/contents")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        // gRPC json renders TypeName as a bare string; the
                        // JSON-RPC era wrapped it in `{name: …}`.
                        .filter_map(|e| {
                            e.as_str().or_else(|| e.pointer("/name").and_then(|n| n.as_str()))
                        })
                        .map(protocol_types::asset::canonicalize_move_type)
                        .collect()
                })
                .unwrap_or_default();
            let pool_ids: Vec<String> = json
                .pointer("/active_pools/contents")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if pool_ids.len() > 1 {
                bail!("custody {cid} tracks {} pools — unexpected for a smoke vault", pool_ids.len());
            }
            let pool = match pool_ids.first() {
                None => None,
                Some(pid_str) => {
                    let pid = ObjectID::from_hex_literal(pid_str)?;
                    let t = client
                        .get_object(pid)
                        .await?
                        .struct_tag()
                        .map(|t| t.to_canonical_string(/* with_prefix */ true))
                        .ok_or_else(|| anyhow!("pool {pid} has no type"))?;
                    let inner = t
                        .split_once('<')
                        .and_then(|(_, i)| i.strip_suffix('>'))
                        .ok_or_else(|| anyhow!("pool {pid} type has no generics"))?;
                    let parts: Vec<&str> = inner.splitn(2, ',').map(str::trim).collect();
                    if parts.len() != 2 {
                        bail!("pool {pid} type parse failed: {t}");
                    }
                    Some((pid, parts[0].to_string(), parts[1].to_string()))
                }
            };
            Some(CustodySnapshot { assets, pool })
        }
    };
    Ok(Some(Plan { state, custody, senior_shares, junior_shares, settled }))
}

/// Dev-inspect a no-arg-beyond-vault `vault::<function>` returning bool.
async fn dev_inspect_bool(
    client: &ChainClient,
    sender: SuiAddress,
    trading_vault_pkg: ObjectID,
    vault_id: ObjectID,
    function: &str,
    extra_pure_u64: &[u64],
) -> Result<bool> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    let mut args = vec![vault];
    for v in extra_pure_u64 {
        args.push(pt.pure(v)?);
    }
    pt.programmable_move_call(
        trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new(function).unwrap(),
        vec![],
        args,
    );
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .with_context(|| format!("dev-inspecting vault::{function}"))?;
    sui_tx::chain::decode_return_value::<bool>(&res, 0)
        .with_context(|| format!("decoding vault::{function}"))
}

/// Everything for one vault in a single atomic PTB: partial progress is
/// impossible, so a failed vault is untouched and the tool can just re-run.
async fn close_vault(
    client: &ChainClient,
    signer: &Signer,
    ids: &Ids,
    cli: &Cli,
    v: &StaleVault,
    state: &str,
    custody: Option<&CustodySnapshot>,
) -> Result<()> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault_arg = pt.obj(shared_object_arg(client, v.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(client, v.cap_id).await?)?;
    let ireg = pt.obj(shared_object_arg(client, ids.integration_registry_id, false).await?)?;
    let clock = clock_arg(&mut pt)?;
    let tv_mod = Identifier::new("vault").unwrap();
    let dba_mod = Identifier::new("deepbook_adapter").unwrap();

    if state == "Open" {
        pt.programmable_move_call(
            ids.trading_vault_pkg,
            tv_mod.clone(),
            Identifier::new("initiate_close").unwrap(),
            vec![],
            vec![vault_arg, cap],
        );
    }

    if let Some(c) = custody {
        let custody_id = v.custody_id.unwrap();
        if let Some((pool_id, base_ty, quote_ty)) = &c.pool {
            let pool = pt.obj(shared_object_arg(client, *pool_id, true).await?)?;
            let generics = vec![TypeTag::from_str(base_ty)?, TypeTag::from_str(quote_ty)?];
            let custody_arg = pt.pure(custody_id)?;
            pt.programmable_move_call(
                ids.deepbook_adapter_pkg,
                dba_mod.clone(),
                Identifier::new("cancel_all_orders").unwrap(),
                generics.clone(),
                vec![vault_arg, cap, ireg, custody_arg, pool, clock],
            );
            let custody_arg = pt.pure(custody_id)?;
            pt.programmable_move_call(
                ids.deepbook_adapter_pkg,
                dba_mod.clone(),
                Identifier::new("withdraw_settled").unwrap(),
                generics.clone(),
                vec![vault_arg, cap, ireg, custody_arg, pool],
            );
            // Sweep every tracked asset back to the vault; zero balances just
            // prune the tag (force sessions are open — the vault is Closing).
            for asset in &c.assets {
                let custody_arg = pt.pure(custody_id)?;
                pt.programmable_move_call(
                    ids.deepbook_adapter_pkg,
                    dba_mod.clone(),
                    Identifier::new("force_sweep").unwrap(),
                    vec![TypeTag::from_str(asset)?],
                    vec![vault_arg, ireg, custody_arg, clock],
                );
            }
            let custody_arg = pt.pure(custody_id)?;
            pt.programmable_move_call(
                ids.deepbook_adapter_pkg,
                dba_mod.clone(),
                Identifier::new("retire_pool").unwrap(),
                generics,
                vec![vault_arg, cap, ireg, custody_arg, pool],
            );
        } else {
            for asset in &c.assets {
                let custody_arg = pt.pure(custody_id)?;
                pt.programmable_move_call(
                    ids.deepbook_adapter_pkg,
                    dba_mod.clone(),
                    Identifier::new("force_sweep").unwrap(),
                    vec![TypeTag::from_str(asset)?],
                    vec![vault_arg, ireg, custody_arg, clock],
                );
            }
        }
        let custody_arg = pt.pure(custody_id)?;
        let recipient = pt.pure(signer.address)?;
        pt.programmable_move_call(
            ids.deepbook_adapter_pkg,
            dba_mod,
            Identifier::new("eject_empty_custody").unwrap(),
            vec![],
            vec![vault_arg, cap, ireg, custody_arg, recipient],
        );
    }

    pt.programmable_move_call(
        ids.trading_vault_pkg,
        tv_mod,
        Identifier::new("finalize_close").unwrap(),
        vec![],
        vec![vault_arg],
    );
    submit_ptb(client, signer, pt, cli.gas_budget, "close::vault").await?;
    Ok(())
}

/// Defensive bound on the queued-request scan: smoke vaults hold a
/// handful of requests at most; anything past this is not ours to drain.
const SEQ_SCAN_CAP: u64 = 256;

/// v2 §8.7: take the one-time settlement snapshot (unless already taken)
/// and drain every outstanding queued withdraw request from the frozen
/// pool. `finalize_close` already asserted only the accounting asset
/// remains, so the snapshot appraisal is cash-only — no oracle legs.
/// Wallet-held positions stay redeemable permissionlessly forever; this
/// tool doesn't chase them.
async fn settle_vault(
    client: &ChainClient,
    signer: &Signer,
    ids: &Ids,
    cli: &Cli,
    v: &StaleVault,
    already_settled: bool,
) -> Result<()> {
    let holdings = sui_tx::tx::appraisal::discover_holdings(client, v.vault_id).await?;
    let deposit_type = holdings.deposit_type.clone();
    let tv_refs = sui_tx::tx::trading_vault::TradingVaultRefs {
        package: ids.trading_vault_pkg,
        vault_id: v.vault_id,
        protocol_config_id: ids.protocol_config_id,
        deposit_type: &deposit_type,
    };

    if !already_settled {
        let refs = sui_tx::tx::appraisal::AppraisalRefs {
            trading_vault_pkg: ids.trading_vault_pkg,
            deepbook_adapter_pkg: Some(ids.deepbook_adapter_pkg),
            options_adapter_pkg: ids.options_adapter_pkg,
            exchange_adapter_pkg: ids.exchange_adapter_pkg,
            vault_id: v.vault_id,
            protocol_config_id: ids.protocol_config_id,
            oracle_registry_id: ids.oracle_registry_id,
            equity_oracle_pkg: ids.equity_oracle_pkg,
            equity_book_id: ids.equity_book_id,
            vol_book_id: ids.vol_book_id,
        };
        let mut pt = ProgrammableTransactionBuilder::new();
        let (appraisal, _) = sui_tx::tx::appraisal::compose_appraisal(
            client,
            &mut pt,
            &refs,
            &holdings,
            None,
            &std::collections::BTreeMap::new(),
            &[],
        )
        .await?;
        sui_tx::tx::trading_vault::build_snapshot_settlement(client, &mut pt, &tv_refs, appraisal)
            .await?;
        submit_ptb(client, signer, pt, cli.gas_budget, "close::snapshot_settlement").await?;
        println!("    ✔ settlement snapshot taken");
    }

    // Outstanding queued requests settle permissionlessly, in any order,
    // once NAV is frozen. The requests table is keyed by global sequence;
    // scan the (tiny) sequence space with `has_request`.
    let (_, json) = client.get_object_json(v.vault_id).await?;
    let json = json.ok_or_else(|| anyhow!("vault {} unreadable", v.vault_id))?;
    let next_seq = json
        .pointer("/next_global_seq")
        .and_then(|x| x.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| x.as_u64()))
        .ok_or_else(|| anyhow!("vault {} has no readable next_global_seq", v.vault_id))?;
    if next_seq > SEQ_SCAN_CAP {
        bail!("vault {} has {next_seq} sequences — beyond the smoke-vault scan cap", v.vault_id);
    }
    let mut outstanding = Vec::new();
    for seq in 0..next_seq {
        if dev_inspect_bool(
            client,
            signer.address,
            ids.trading_vault_pkg,
            v.vault_id,
            "has_request",
            &[seq],
        )
        .await?
        {
            outstanding.push(seq);
        }
    }
    if outstanding.is_empty() {
        return Ok(());
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    for seq in &outstanding {
        sui_tx::tx::trading_vault::build_settle_queued_request(
            client,
            &mut pt,
            &tv_refs,
            ids.treasury_id,
            *seq,
        )
        .await?;
    }
    submit_ptb(client, signer, pt, cli.gas_budget, "close::settle_queued_requests").await?;
    println!("    ✔ settled {} queued request(s)", outstanding.len());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = ChainClient::new(&cli.rpc)?;
    let signer = load_signer(&cli.address)?;
    let ids = resolve_ids(&cli)?;
    println!(
        "trading-vault close — signer {}, env {}{}",
        signer.address,
        cli.env,
        if cli.execute { "" } else { " (dry run)" },
    );

    let stale = enumerate_stale(&cli, signer.address).await?;
    println!("{} stale open vault(s) to close", stale.len());

    let (mut closed, mut skipped, mut failed) = (0u32, 0u32, 0u32);
    for v in &stale {
        let plan = match verify_on_chain(&client, &ids, signer.address, cli.max_shares, v).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                println!("  {}: already Closed and settled on-chain — skipping", v.vault_id);
                skipped += 1;
                continue;
            }
            Err(e) => {
                println!("  {}: SKIP — {e:#}", v.vault_id);
                skipped += 1;
                continue;
            }
        };
        let desc = match &plan.custody {
            Some(c) => format!(
                "custody: {} asset(s){}",
                c.assets.len(),
                c.pool
                    .as_ref()
                    .map(|(p, ..)| format!(", pool {}", &p.to_string()[..10]))
                    .unwrap_or_default(),
            ),
            None => "no position".into(),
        };
        println!(
            "  {} [{}, {}s/{}j shares] — {desc}",
            v.vault_id, plan.state, plan.senior_shares, plan.junior_shares
        );
        if !cli.execute {
            continue;
        }
        let result = async {
            if plan.state != "Closed" {
                close_vault(&client, &signer, &ids, &cli, v, &plan.state, plan.custody.as_ref())
                    .await?;
            }
            // v2: Closed is not done — settle (§8.7).
            settle_vault(&client, &signer, &ids, &cli, v, plan.settled).await
        }
        .await;
        match result {
            Ok(()) => {
                println!("    ✔ closed + settled");
                closed += 1;
            }
            Err(e) => {
                println!("    ✘ FAILED: {e:#}");
                failed += 1;
            }
        }
    }
    println!("done: {closed} closed, {skipped} skipped, {failed} failed");
    if failed > 0 {
        bail!("{failed} vault(s) failed to close — re-run after investigating");
    }
    Ok(())
}
