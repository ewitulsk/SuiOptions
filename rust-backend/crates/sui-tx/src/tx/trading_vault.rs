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
use sui_types::base_types::ObjectID;
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
    /// The vault's ACCOUNTING asset type (canonical `0x…::mod::TYPE`) —
    /// the unit of account (SO-370 renamed the Move field; deposits may
    /// be any allowlisted asset, but these builders' begin_appraisal /
    /// fulfill anchors always take the accounting asset).
    pub deposit_type: &'a str,
}

impl TradingVaultRefs<'_> {
    fn deposit_tag(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![TypeTag::from_str(self.deposit_type)
            .with_context(|| format!("parsing deposit type {}", self.deposit_type))?])
    }

    /// `{package}::price::PriceAttestation` — the option/vector element
    /// type every attestation-bearing call names.
    fn attestation_tag(&self) -> Result<TypeTag> {
        TypeTag::from_str(&format!("{}::price::PriceAttestation", self.package))
            .context("attestation type tag")
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

/// `0x1::option::none<PriceAttestation>()` — the accounting-asset
/// deposit's empty attestation slot.
fn none_attestation(
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
) -> Result<Argument> {
    Ok(pt.programmable_move_call(
        ObjectID::from_hex_literal("0x1").unwrap(),
        Identifier::new("option").unwrap(),
        Identifier::new("none").unwrap(),
        vec![refs.attestation_tag()?],
        vec![],
    ))
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

/// `vault::deposit<T>(vault, cfg, core_cfg, appraisal, coin, att, clock)`
/// for the ACCOUNTING asset only: the attestation option is `none`
/// (SO-370). `core_config_id` is the options_core `ProtocolConfig` — the
/// ingress whitelist gate (SO-383). Attestation-bearing (non-accounting)
/// deposits are composed through the appraisal composer, which returns the
/// attest result the option wraps.
pub async fn build_deposit(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    core_config_id: ObjectID,
    appraisal: Argument,
    funds: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let core_cfg = pt.obj(shared_object_arg(client, core_config_id, false).await?)?;
    let att = none_attestation(pt, refs)?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "deposit",
        refs.deposit_tag()?,
        vec![vault, cfg, core_cfg, appraisal, funds, att, clock],
    );
    Ok(())
}

/// `vault::deposit<A>(vault, cfg, core_cfg, appraisal, coin, option::some(att), clock)`
/// for a NON-accounting allowlisted asset (SO-370). `att` is the
/// composer-emitted `PriceAttestation` for `asset_type` (attestations are
/// `copy`, so the appraisal legs and this option share the same result);
/// `appraisal` must have been composed with `asset_type` in
/// `extra_attest` when the vault doesn't hold it yet. `core_config_id` is
/// the options_core `ProtocolConfig` (ingress whitelist gate, SO-383).
pub async fn build_deposit_asset(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    core_config_id: ObjectID,
    asset_type: &str,
    appraisal: Argument,
    funds: Argument,
    att: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let core_cfg = pt.obj(shared_object_arg(client, core_config_id, false).await?)?;
    let some_att = pt.programmable_move_call(
        ObjectID::from_hex_literal("0x1").unwrap(),
        Identifier::new("option").unwrap(),
        Identifier::new("some").unwrap(),
        vec![refs.attestation_tag()?],
        vec![att],
    );
    let clock = clock_arg(pt)?;
    let asset_tag = TypeTag::from_str(asset_type)
        .with_context(|| format!("parsing deposit asset type {asset_type}"))?;
    vault_call(
        pt,
        refs.package,
        "deposit",
        vec![asset_tag],
        vec![vault, cfg, core_cfg, appraisal, funds, some_att, clock],
    );
    Ok(())
}

/// `vault::request_withdraw<P>(vault, shares, clock)` — no appraisal.
/// `payout_type` is the allowlisted asset the recipient wants to be paid
/// in (SO-370); the accounting asset is always legal.
pub async fn build_request_withdraw(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    payout_type: &str,
    shares: u128,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let shares = pt.pure(&shares)?;
    let clock = clock_arg(pt)?;
    let payout = TypeTag::from_str(payout_type)
        .with_context(|| format!("parsing payout type {payout_type}"))?;
    vault_call(pt, refs.package, "request_withdraw", vec![payout], vec![vault, shares, clock]);
    Ok(())
}

/// `vault::amend_payout_asset<P>(vault, seq)` — the recipient re-points a
/// pending request's payout asset (SO-370's unwedge lever).
pub async fn build_amend_payout_asset(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    payout_type: &str,
    seq: u64,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let seq = pt.pure(&seq)?;
    let payout = TypeTag::from_str(payout_type)
        .with_context(|| format!("parsing payout type {payout_type}"))?;
    vault_call(pt, refs.package, "amend_payout_asset", vec![payout], vec![vault, seq]);
    Ok(())
}

/// `vault::fulfill_withdrawals<T>(vault, cfg, treasury, appraisal, clock)`
/// — the keeper crank tail for ACCOUNTING-payable heads (requested in the
/// accounting asset, or aged past the grace fallback); prepend
/// `build_begin_appraisal` (+ attestation legs when the vault holds more
/// than cash). Mixed-asset runs go through [`build_fulfill_mixed`].
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
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "fulfill_withdrawals",
        refs.deposit_tag()?,
        vec![vault, cfg, treasury, appraisal, clock],
    );
    Ok(())
}

/// The fulfillment-potato chain for a mixed-asset queue run (SO-370):
///
///   begin_fulfillment(vault, cfg, appraisal, atts, clock)
///   fulfill_next<P>(vault, cfg, treasury, &mut f, clock) × plan
///   end_fulfillment(vault, f)
///
/// `atts` are `PriceAttestation` arguments — one per distinct
/// NON-accounting payout asset the run will pay, each quoting into the
/// accounting asset (reuse the appraisal composer's attest results;
/// attestations are `copy`). `plan` is the FIFO-ordered
/// `(payout_coin_type, count)` chain; `fulfill_next` returning false is a
/// NO-OP (wrong asset / unfundable head), so a speculative chain is safe.
pub async fn build_fulfill_mixed(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    treasury_id: ObjectID,
    appraisal: Argument,
    atts: Vec<Argument>,
    plan: &[(String, usize)],
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    let clock = clock_arg(pt)?;
    let atts_vec = pt.command(sui_types::transaction::Command::MakeMoveVec(
        Some(refs.attestation_tag()?.into()),
        atts,
    ));
    let f = vault_call(
        pt,
        refs.package,
        "begin_fulfillment",
        vec![],
        vec![vault, cfg, appraisal, atts_vec, clock],
    );
    for (payout_type, count) in plan {
        let payout = TypeTag::from_str(payout_type)
            .with_context(|| format!("parsing payout type {payout_type}"))?;
        for _ in 0..*count {
            vault_call(
                pt,
                refs.package,
                "fulfill_next",
                vec![payout.clone()],
                vec![vault, cfg, treasury, f, clock],
            );
        }
    }
    vault_call(pt, refs.package, "end_fulfillment", vec![], vec![vault, f]);
    Ok(())
}

/// `vault::crank_appraisal(vault, appraisal)` — permissionless mark
/// refresh (SO-304): validates and discards the appraisal so the
/// PositionAppraised / VaultAppraised events carry fresh marks with no
/// deposit/fulfillment attached. Type-free since SO-370.
pub async fn build_crank_appraisal(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    vault_call(pt, refs.package, "crank_appraisal", vec![], vec![vault, appraisal]);
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
/// the Move parameter list. The `CuratorCap` always lands with the tx
/// sender: the creator IS the initial curator.
#[derive(Debug, Clone, Copy)]
pub struct CreateVaultSpec {
    pub lockup_ms: u64,
    pub curator_fee_bps: u64,
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
    core_config_id: ObjectID,
    deposit_type: &str,
    spec: &CreateVaultSpec,
    gas_budget: u64,
) -> Result<VaultCreation> {
    info!(%package, deposit_type, "building create_vault PTB");
    let deposit_tag = TypeTag::from_str(deposit_type)
        .with_context(|| format!("parsing deposit type {deposit_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let cfg = pt.obj(shared_object_arg(client, protocol_config_id, false).await?)?;
    // options_core ProtocolConfig — the ingress whitelist gate (SO-383).
    let core_cfg = pt.obj(shared_object_arg(client, core_config_id, false).await?)?;
    let args = vec![
        cfg,
        core_cfg,
        pt.pure(&spec.lockup_ms)?,
        pt.pure(&spec.curator_fee_bps)?,
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

/// Dev-inspect `vault::free_balance_of<T>` — the vault's free balance in
/// `coin_type` smallest units (0 when the balance df was pruned). Read
/// path only; used by direct-escrow quoters sizing against vault capital
/// (SO-372).
pub async fn dev_inspect_free_balance(
    client: &ChainClient,
    sender: sui_types::base_types::SuiAddress,
    package: ObjectID,
    vault_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    let tag = TypeTag::from_str(coin_type)
        .with_context(|| format!("parsing coin type {coin_type}"))?;
    vault_call(&mut pt, package, "free_balance_of", vec![tag], vec![vault]);
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting free_balance_of")?;
    crate::chain::decode_return_value::<u64>(&res, 0).context("decoding free_balance_of")
}
