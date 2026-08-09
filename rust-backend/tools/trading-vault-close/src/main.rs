//! Close stale curated trading vaults on a live network (SO-361).
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
//! Safety rails: only vaults where this wallet is creator AND current curator,
//! whose every position is a DeepBookCustody (the live desk vault holds a
//! VaultMm position, so it can never match), and whose share supply is at
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
    integration_registry_id: ObjectID,
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
        integration_registry_id: objs.integration_registry()?,
    })
}

/// One open vault from the indexer, with its single custody position.
struct StaleVault {
    vault_id: ObjectID,
    cap_id: ObjectID,
    shares: u128,
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
    let resp = gql(
        &cli.indexer_graphql,
        "{ tradingVaults { vaultId state creator curator curatorCapId totalSharesRaw } }",
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
        if get("state") != Some("open") {
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
        let shares: u128 = get("totalSharesRaw").unwrap_or("0").parse().unwrap_or(u128::MAX);
        if shares > cli.max_shares {
            println!("  skip {vid}: {shares} shares > --max-shares (the live desk vault?)");
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
            shares,
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

async fn verify_on_chain(
    client: &ChainClient,
    me: SuiAddress,
    v: &StaleVault,
) -> Result<Option<(String, Option<CustodySnapshot>)>> {
    let (_, vault_json) = client.get_object_json(v.vault_id).await?;
    let vault_json = vault_json.ok_or_else(|| anyhow!("vault {} unreadable", v.vault_id))?;
    let state = vault_json
        .pointer("/state")
        .and_then(enum_variant)
        .ok_or_else(|| anyhow!("vault {} has no readable state", v.vault_id))?
        .to_string();
    if state == "Closed" {
        return Ok(None);
    }
    if vault_json.pointer("/creator").and_then(|x| x.as_str()) != Some(&me.to_string()) {
        bail!("vault {} on-chain creator is not the signer", v.vault_id);
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
                        .map(|(_, i)| i.trim_end_matches('>'))
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
    Ok(Some((state, custody)))
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
        let plan = match verify_on_chain(&client, signer.address, v).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                println!("  {}: already Closed on-chain — skipping", v.vault_id);
                skipped += 1;
                continue;
            }
            Err(e) => {
                println!("  {}: SKIP — {e:#}", v.vault_id);
                skipped += 1;
                continue;
            }
        };
        let (state, custody) = plan;
        let desc = match &custody {
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
        println!("  {} [{state}, {} shares] — {desc}", v.vault_id, v.shares);
        if !cli.execute {
            continue;
        }
        match close_vault(&client, &signer, &ids, &cli, v, &state, custody.as_ref()).await {
            Ok(()) => {
                println!("    ✔ closed");
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
