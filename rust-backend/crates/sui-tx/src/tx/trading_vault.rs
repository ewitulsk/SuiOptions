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

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::{clock_arg, shared_object_arg};

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
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    Ok(vault_call(pt, refs.package, "begin_appraisal", refs.deposit_tag()?, vec![vault]))
}

/// `vault::deposit<T>(vault, cfg, appraisal, coin, clock)`.
pub async fn build_deposit(
    client: &SuiClient,
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
    client: &SuiClient,
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
    client: &SuiClient,
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
    client: &SuiClient,
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
    client: &SuiClient,
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
