//! PTB builders for the curated trading vault (`trading_vault::vault`) —
//! the user flows the frontend templates wrap (deposit / withdrawal
//! queue) and the permissionless fulfillment crank the keeper submits.
//!
//! Deposits and fulfillment consume an `Appraisal` hot potato that must
//! cover every held asset type and custodied position. These builders
//! cover the CASH case (deposit asset only, no positions): begin →
//! consume in one PTB with no attestation legs. Attestation-bearing
//! appraisals (multi-asset vaults, open positions) are composed by the
//! frontend/keeper by inserting `oracle_pyth::attest` +
//! `vault::appraise_balance` + adapter `appraise_*` calls between the
//! two anchors; the gas-station template allows those legs.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use tracing::info;

use crate::tx::{clock_arg, shared_object_arg};
use crate::chain::{created_objects, ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;

/// Identity of one trading vault and the shared protocol objects its
/// calls need.
pub struct TradingVaultRefs<'a> {
    /// The `trading_vault` package id (token-info `snapshot.trading_vault()`).
    pub package: ObjectID,
    pub vault_id: ObjectID,
    /// Shared `VaultProtocolConfig`.
    pub protocol_config_id: ObjectID,
    /// The vault's deposit asset type (canonical `0x…::mod::TYPE`).
    pub deposit_type: &'a str,
}

impl TradingVaultRefs<'_> {
    fn deposit_tag(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![TypeTag::from_str(self.deposit_type)
            .with_context(|| format!("parsing deposit type {}", self.deposit_type))?])
    }
}

fn vault_call(
    pt: &mut ProgrammableTransactionBuilder,
    package: ObjectID,
    function: &str,
    tags: Vec<TypeTag>,
    args: Vec<Argument>,
) -> Argument {
    pt.programmable_move_call(
        package,
        Identifier::new("vault").unwrap(),
        Identifier::new(function).unwrap(),
        tags,
        args,
    )
}

/// `vault::begin_appraisal<T>(&vault)` — returns the Appraisal argument
/// to thread into `build_deposit` / `build_fulfill_withdrawals` (with
/// any attestation legs in between).
pub async fn build_begin_appraisal(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    Ok(vault_call(pt, refs.package, "begin_appraisal", refs.deposit_tag()?, vec![vault]))
}

/// `vault::deposit<T>(vault, cfg, appraisal, coin, clock)`.
pub async fn build_deposit(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
    funds: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "deposit",
        refs.deposit_tag()?,
        vec![vault, cfg, appraisal, funds, clock],
    );
    Ok(())
}

/// `vault::request_withdraw(vault, shares, clock)` — no appraisal.
pub async fn build_request_withdraw(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    shares: u128,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let shares = pt.pure(&shares)?;
    let clock = clock_arg(pt)?;
    vault_call(pt, refs.package, "request_withdraw", vec![], vec![vault, shares, clock]);
    Ok(())
}

/// `vault::fulfill_withdrawals<T>(vault, cfg, treasury, appraisal)` —
/// the keeper crank tail; prepend `build_begin_appraisal` (+ attestation
/// legs when the vault holds more than cash).
pub async fn build_fulfill_withdrawals(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    treasury_id: ObjectID,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    vault_call(
        pt,
        refs.package,
        "fulfill_withdrawals",
        refs.deposit_tag()?,
        vec![vault, cfg, treasury, appraisal],
    );
    Ok(())
}

/// `vault::crank_appraisal<T>(vault, appraisal)` — permissionless mark
/// refresh (SO-304): validates and discards the appraisal so the
/// PositionAppraised / VaultAppraised events carry fresh marks with no
/// deposit/fulfillment attached.
pub async fn build_crank_appraisal(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    vault_call(pt, refs.package, "crank_appraisal", refs.deposit_tag()?, vec![vault, appraisal]);
    Ok(())
}

/// `vault::enqueue_closed_stake(vault, owner, clock)` — permissionless
/// closed-vault distribution.
pub async fn build_enqueue_closed_stake(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    owner: sui_types::base_types::SuiAddress,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let owner = pt.pure(&owner)?;
    let clock = clock_arg(pt)?;
    vault_call(pt, refs.package, "enqueue_closed_stake", vec![], vec![vault, owner, clock]);
    Ok(())
}

// ── curator provisioning (SO-345) ──────────────────────────────────────

/// `vault::create_vault<T>`'s config arguments. Order matters — it mirrors
/// the Move parameter list.
#[derive(Debug, Clone, Copy)]
pub struct CreateVaultSpec {
    /// Who receives the `CuratorCap`. `create_vault` is permissionless and
    /// this is a free parameter, so a cap naming you curator proves nothing
    /// about who made the vault — only `creator` (the tx sender) does.
    pub curator: SuiAddress,
    pub lockup_ms: u64,
    pub curator_fee_bps: u64,
    /// 0 = creator rotates, 1 = curator rotates, 2 = either.
    pub rotation_authority: u8,
    pub max_positions: u64,
    pub unwind_grace_ms: u64,
}

pub struct VaultCreation {
    pub digest: String,
    pub vault_id: ObjectID,
    pub curator_cap_id: ObjectID,
}

/// Create a curated trading vault. Permissionless: no AdminCap, no share
/// coin to publish (shares are ledger entries, not a `Coin`), and no seed
/// deposit required — with no donation path in, NAV cannot be inflated
/// ahead of the first depositor.
///
/// The vault lands with `mm_release_enabled = false`; the curator has to
/// turn it on before `vault_mm::release` will serve a quote.
pub async fn create_vault(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    protocol_config_id: ObjectID,
    deposit_type: &str,
    spec: &CreateVaultSpec,
    gas_budget: u64,
) -> Result<VaultCreation> {
    info!(%package, deposit_type, curator = %spec.curator, "building create_vault PTB");
    let deposit_tag = TypeTag::from_str(deposit_type)
        .with_context(|| format!("parsing deposit type {deposit_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let cfg = pt.obj(shared_object_arg(client, protocol_config_id, false).await?)?;
    let args = vec![
        cfg,
        pt.pure(&spec.curator)?,
        pt.pure(&spec.lockup_ms)?,
        pt.pure(&spec.curator_fee_bps)?,
        pt.pure(&spec.rotation_authority)?,
        pt.pure(&spec.max_positions)?,
        pt.pure(&spec.unwind_grace_ms)?,
    ];
    vault_call(&mut pt, package, "create_vault", vec![deposit_tag], args);

    let resp = super::submit_ptb(client, signer, pt, gas_budget, "vault::create_vault").await?;
    let created = created_objects(&resp);
    let find = |name: &str| {
        created.iter().find_map(|c| {
            let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
            (tag.module.as_str() == "vault" && tag.name.as_str() == name).then_some(c.object_id)
        })
    };
    let vault_id = find("TradingVault")
        .ok_or_else(|| anyhow!("create_vault succeeded but no TradingVault in ObjectChanges"))?;
    let curator_cap_id = find("CuratorCap")
        .ok_or_else(|| anyhow!("create_vault succeeded but no CuratorCap in ObjectChanges"))?;

    // Do not hand back ids the fullnode cannot serve yet. Callers build
    // their next PTB from a read of these two, and that read 404s until
    // the read view catches up with the write we just made.
    client.await_object(vault_id, 6).await.context("waiting for the new vault to be readable")?;
    client
        .await_object(curator_cap_id, 6)
        .await
        .context("waiting for the new CuratorCap to be readable")?;

    Ok(VaultCreation {
        digest: super::tx_digest(&resp).to_string(),
        vault_id,
        curator_cap_id,
    })
}

/// `vault::set_mm_release_enabled(vault, cap, enabled)` — the curator gate
/// on `vault_mm::release`. Quotes revert while it is off.
///
/// Submitted with a rebuild-per-attempt because the `CuratorCap` is an
/// owned object this wallet mutates on every curator-session tx, so its
/// reference goes stale exactly the way the scheduler's AdminCap did.
pub async fn set_mm_release_enabled(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    vault_id: ObjectID,
    curator_cap: ObjectID,
    enabled: bool,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    super::submit_ptb_rebuilding(
        client,
        signer,
        gas_budget,
        "vault::set_mm_release_enabled",
        || async {
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let cap = pt.obj(super::owned_object_arg(client, curator_cap).await?)?;
            let flag = pt.pure(&enabled)?;
            vault_call(&mut pt, package, "set_mm_release_enabled", vec![], vec![vault, cap, flag]);
            Ok(pt.finish())
        },
    )
    .await
}
