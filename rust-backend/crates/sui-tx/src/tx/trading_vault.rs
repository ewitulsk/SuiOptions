//! PTB builders for the curated trading vault (`vault_v2::vault`, served
//! under the `tradingVault` deployment key) — the user flows the frontend
//! templates wrap (deposit / withdrawal queue / position split-merge /
//! settlement redemption) and the permissionless cranks the keeper
//! submits.
//!
//! v2 (SO-418): deposits mint a transferable `VaultPosition` NFT — the
//! deposit builders RETURN its `Argument` so callers compose
//! `TransferObjects` (or use the `_and_transfer` conveniences).
//! Withdrawal requests consume a whole position object; partial exits
//! split first. Closed vaults settle through the settlement pool
//! (`snapshot_settlement` → `redeem_settled_position` /
//! `settle_queued_request`) — `enqueue_closed_stake` is a deleted
//! concept.
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
use serde::Deserialize;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use tracing::info;

use crate::tx::{clock_arg, owned_object_arg, shared_object_arg};
use crate::chain::{created_objects, ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;

/// Identity of one trading vault and the shared protocol objects its
/// calls need.
pub struct TradingVaultRefs<'a> {
    /// The vault package id (token-info `snapshot.trading_vault()` — the
    /// on-chain package is `vault_v2` since SO-418; the key is unchanged).
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

/// Same shape against the `vault_position` module (split / merge — the
/// NFT's own ops, no vault object involved).
fn position_call(
    pt: &mut ProgrammableTransactionBuilder,
    package: ObjectID,
    function: &str,
    args: Vec<Argument>,
) -> Argument {
    pt.programmable_move_call(
        package,
        Identifier::new("vault_position").unwrap(),
        Identifier::new(function).unwrap(),
        vec![],
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

/// `vault::deposit<T>(vault, cfg, wl, appraisal, coin, att, tranche_code,
/// clock): VaultPosition` for the ACCOUNTING asset only: the attestation
/// option is `none` (SO-370). `whitelist_id` is the shared
/// `whitelist::Whitelist` — the ingress gate (SO-383). `tranche_code` is
/// the wire code (0 untranched / 1 senior / 2 junior). Returns the
/// minted `VaultPosition` Argument — the caller MUST consume it
/// (`TransferObjects`, or feed it to another call) or the tx fails; see
/// [`build_deposit_and_transfer`]. Attestation-bearing (non-accounting)
/// deposits are composed through the appraisal composer, which returns
/// the attest result the option wraps.
pub async fn build_deposit(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    appraisal: Argument,
    funds: Argument,
    tranche_code: u8,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, whitelist_id, false).await?)?;
    let att = none_attestation(pt, refs)?;
    let tranche = pt.pure(&tranche_code)?;
    let clock = clock_arg(pt)?;
    Ok(vault_call(
        pt,
        refs.package,
        "deposit",
        refs.deposit_tag()?,
        vec![vault, cfg, wl, appraisal, funds, att, tranche, clock],
    ))
}

/// [`build_deposit`] + a trailing `TransferObjects` of the minted
/// `VaultPosition` to `recipient` — the standalone deposit shape
/// (mirrors the sponsored frontend PTB).
#[allow(clippy::too_many_arguments)]
pub async fn build_deposit_and_transfer(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    appraisal: Argument,
    funds: Argument,
    tranche_code: u8,
    recipient: SuiAddress,
) -> Result<()> {
    let position =
        build_deposit(client, pt, refs, whitelist_id, appraisal, funds, tranche_code).await?;
    pt.transfer_arg(recipient, position);
    Ok(())
}

/// `vault::deposit<A>(vault, cfg, wl, appraisal, coin, option::some(att),
/// tranche_code, clock): VaultPosition` for a NON-accounting allowlisted
/// asset (SO-370). `att` is the composer-emitted `PriceAttestation` for
/// `asset_type` (attestations are `copy`, so the appraisal legs and this
/// option share the same result); `appraisal` must have been composed
/// with `asset_type` in `extra_attest` when the vault doesn't hold it
/// yet. `whitelist_id` is the shared `whitelist::Whitelist` (ingress
/// gate, SO-383). Returns the minted `VaultPosition` Argument the caller
/// must consume.
#[allow(clippy::too_many_arguments)]
pub async fn build_deposit_asset(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    asset_type: &str,
    appraisal: Argument,
    funds: Argument,
    att: Argument,
    tranche_code: u8,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, whitelist_id, false).await?)?;
    let some_att = pt.programmable_move_call(
        ObjectID::from_hex_literal("0x1").unwrap(),
        Identifier::new("option").unwrap(),
        Identifier::new("some").unwrap(),
        vec![refs.attestation_tag()?],
        vec![att],
    );
    let tranche = pt.pure(&tranche_code)?;
    let clock = clock_arg(pt)?;
    let asset_tag = TypeTag::from_str(asset_type)
        .with_context(|| format!("parsing deposit asset type {asset_type}"))?;
    Ok(vault_call(
        pt,
        refs.package,
        "deposit",
        vec![asset_tag],
        vec![vault, cfg, wl, appraisal, funds, some_att, tranche, clock],
    ))
}

/// `vault::deposit_into_commitment<T>(vault, cfg, wl, cap, appraisal,
/// coin, att, clock)` — curator commitment funding (§8.6): same
/// valuation and share math as `deposit`, but the claim lands in (or
/// merges into) the in-vault escrowed commitment position, so nothing is
/// returned. ACCOUNTING asset only (attestation `none`); `curator_cap`
/// is the curator's owned `CuratorCap`.
pub async fn build_deposit_into_commitment(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    curator_cap: ObjectID,
    appraisal: Argument,
    funds: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, whitelist_id, false).await?)?;
    let cap = pt.obj(owned_object_arg(client, curator_cap).await?)?;
    let att = none_attestation(pt, refs)?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "deposit_into_commitment",
        refs.deposit_tag()?,
        vec![vault, cfg, wl, cap, appraisal, funds, att, clock],
    );
    Ok(())
}

/// `vault::request_withdraw<P>(vault, position, clock)` — consumes a
/// whole `VaultPosition` object (v2); partial exits `split` first.
/// No appraisal. `payout_type` is the allowlisted asset the recipient
/// wants to be paid in (SO-370); the accounting asset is always legal.
/// `position_id` is the caller's owned position NFT.
pub async fn build_request_withdraw(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    payout_type: &str,
    position_id: ObjectID,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let position = pt.obj(owned_object_arg(client, position_id).await?)?;
    let clock = clock_arg(pt)?;
    let payout = TypeTag::from_str(payout_type)
        .with_context(|| format!("parsing payout type {payout_type}"))?;
    vault_call(pt, refs.package, "request_withdraw", vec![payout], vec![vault, position, clock]);
    Ok(())
}

/// `vault::amend_payout_asset<P>(vault, global_seq)` — the recipient
/// re-points a pending request's payout asset (SO-370's unwedge lever).
/// `seq` is the request's GLOBAL sequence (v2 lanes share one sequence).
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

// ── position ops (v2 NFT claims) ───────────────────────────────────────

/// `vault_position::split(&mut position, shares): VaultPosition` — carve
/// `shares` out of an owned position into a new NFT (basis pro rata,
/// same lock/tranche/generation). Returns the child's Argument — the
/// caller must transfer or consume it.
pub async fn build_split_position(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    position_id: ObjectID,
    shares: u128,
) -> Result<Argument> {
    let position = pt.obj(owned_object_arg(client, position_id).await?)?;
    let shares = pt.pure(&shares)?;
    Ok(position_call(pt, refs.package, "split", vec![position, shares]))
}

/// `vault_position::merge(&mut position, other)` — merge `other` into
/// `position` (same vault/tranche/generation; shares and basis add, lock
/// takes the max). `other` is consumed.
pub async fn build_merge_positions(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    position_id: ObjectID,
    other_id: ObjectID,
) -> Result<()> {
    let position = pt.obj(owned_object_arg(client, position_id).await?)?;
    let other = pt.obj(owned_object_arg(client, other_id).await?)?;
    position_call(pt, refs.package, "merge", vec![position, other]);
    Ok(())
}

/// `vault::burn_wiped_position(&vault, position)` — burn a junior
/// position from a wiped generation (its claim is permanently zero after
/// a junior reset, §8.5). Aborts unless the position really is wiped.
pub async fn build_burn_wiped_position(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    position_id: ObjectID,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    let position = pt.obj(owned_object_arg(client, position_id).await?)?;
    vault_call(pt, refs.package, "burn_wiped_position", vec![], vec![vault, position]);
    Ok(())
}

// ── fulfillment ────────────────────────────────────────────────────────

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
/// v2: `begin_fulfillment` takes the vault MUTABLY (it syncs capital and
/// locks both tranche crystallization ratios) — the single shared vault
/// input below is already mutable. The contract picks lane heads itself
/// ("lowest payable global sequence"), so `plan` stays the
/// `(payout_coin_type, count)` chain — the keeper plans counts per lane.
///
/// `atts` are `PriceAttestation` arguments — one per distinct
/// NON-accounting payout asset the run will pay, each quoting into the
/// accounting asset (reuse the appraisal composer's attest results;
/// attestations are `copy`). `fulfill_next` returning false is a NO-OP
/// (wrong asset / blocked lane / unfundable head), so a speculative
/// chain is safe.
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

// ── cranks (permissionless) ────────────────────────────────────────────

/// `vault::crank_appraisal(&vault, appraisal)` — permissionless mark
/// refresh (SO-304): validates and discards the appraisal so the
/// PositionAppraised / VaultAppraised events carry fresh marks with no
/// deposit/fulfillment attached. Type-free since SO-370; immutable in v2
/// too (it cannot move value or skew a snapshot).
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

/// `vault::crank_capital(&mut vault, cfg, appraisal, clock)` —
/// permissionless capital sync (v2): hurdle accrual, waterfall,
/// risk-state transition, commitment test. The keeper's cadence call —
/// the hurdle accrual cap makes its cadence a correctness obligation
/// (contract plan §3.3/§9.4).
pub async fn build_crank_capital(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let clock = clock_arg(pt)?;
    vault_call(pt, refs.package, "crank_capital", vec![], vec![vault, cfg, appraisal, clock]);
    Ok(())
}

// ── junior reset (§8.5) ────────────────────────────────────────────────

/// `vault::propose_junior_reset(&mut vault, cfg, appraisal, clock)` —
/// permissionless: start the timelocked junior-generation reset once the
/// vault is impaired past the threshold.
pub async fn build_propose_junior_reset(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let clock = clock_arg(pt)?;
    vault_call(pt, refs.package, "propose_junior_reset", vec![], vec![vault, cfg, appraisal, clock]);
    Ok(())
}

/// `vault::execute_junior_reset<T>(vault, cfg, wl, appraisal, funds,
/// clock): VaultPosition` — recapitalize a wiped junior tranche with
/// fresh accounting-asset `funds`, minting the new-generation position.
/// Ingress-gated (`whitelist_id`). Returns the position Argument the
/// caller must consume; see
/// [`build_execute_junior_reset_and_transfer`].
pub async fn build_execute_junior_reset(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    appraisal: Argument,
    funds: Argument,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, whitelist_id, false).await?)?;
    let clock = clock_arg(pt)?;
    Ok(vault_call(
        pt,
        refs.package,
        "execute_junior_reset",
        refs.deposit_tag()?,
        vec![vault, cfg, wl, appraisal, funds, clock],
    ))
}

/// [`build_execute_junior_reset`] + `TransferObjects` of the minted
/// position to `recipient`.
pub async fn build_execute_junior_reset_and_transfer(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    appraisal: Argument,
    funds: Argument,
    recipient: SuiAddress,
) -> Result<()> {
    let position =
        build_execute_junior_reset(client, pt, refs, whitelist_id, appraisal, funds).await?;
    pt.transfer_arg(recipient, position);
    Ok(())
}

// ── curator commitment escrow (§8.6) ───────────────────────────────────

/// `vault::release_commitment(vault, cap, cfg, appraisal, shares, clock):
/// VaultPosition` — split `shares` (0 = ALL) out of the escrowed
/// commitment into an ordinary transferable position. While Open the
/// release must leave the marked commitment at/above the floor and is
/// blocked outright when risk-off. Returns the Argument the caller must
/// consume; see [`build_release_commitment_and_transfer`].
pub async fn build_release_commitment(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    curator_cap: ObjectID,
    appraisal: Argument,
    shares: u128,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(client, curator_cap).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let shares = pt.pure(&shares)?;
    let clock = clock_arg(pt)?;
    Ok(vault_call(
        pt,
        refs.package,
        "release_commitment",
        vec![],
        vec![vault, cap, cfg, appraisal, shares, clock],
    ))
}

/// [`build_release_commitment`] + `TransferObjects` of the released
/// position to `recipient`.
pub async fn build_release_commitment_and_transfer(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    curator_cap: ObjectID,
    appraisal: Argument,
    shares: u128,
    recipient: SuiAddress,
) -> Result<()> {
    let position =
        build_release_commitment(client, pt, refs, curator_cap, appraisal, shares).await?;
    pt.transfer_arg(recipient, position);
    Ok(())
}

/// `vault::withdraw_commitment_settled(vault, cap): VaultPosition` —
/// once Closed && settled, hand the escrowed commitment back to the cap
/// holder (no floor, no appraisal — NAV is frozen). Returns the position
/// Argument the caller must consume (typically feed it straight to
/// [`build_redeem_settled_position`] — note that one takes an OWNED
/// object id, so compose the transfer here and redeem next tx, or
/// transfer to self).
pub async fn build_withdraw_commitment_settled(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    curator_cap: ObjectID,
) -> Result<Argument> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(client, curator_cap).await?)?;
    Ok(vault_call(pt, refs.package, "withdraw_commitment_settled", vec![], vec![vault, cap]))
}

// ── terminal settlement (§8.7) ─────────────────────────────────────────

/// `vault::snapshot_settlement(&mut vault, cfg, appraisal, clock)` —
/// permissionless: freeze terminal entitlements on a Closed vault (the
/// keeper's one-shot settlement crank).
pub async fn build_snapshot_settlement(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    appraisal: Argument,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let clock = clock_arg(pt)?;
    vault_call(pt, refs.package, "snapshot_settlement", vec![], vec![vault, cfg, appraisal, clock]);
    Ok(())
}

/// `vault::redeem_settled_position<T>(vault, cfg, treasury, position)` —
/// redeem a wallet-held position directly against the frozen settlement
/// pool: no queue, no appraisal, no keeper. Payout lands with the
/// sender. `T` is the accounting asset.
pub async fn build_redeem_settled_position(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    treasury_id: ObjectID,
    position_id: ObjectID,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    let position = pt.obj(owned_object_arg(client, position_id).await?)?;
    vault_call(
        pt,
        refs.package,
        "redeem_settled_position",
        refs.deposit_tag()?,
        vec![vault, cfg, treasury, position],
    );
    Ok(())
}

/// `vault::settle_queued_request<T>(vault, cfg, treasury, global_seq)` —
/// permissionless: pay an outstanding queued request from the settlement
/// pool at the snapshot entitlement (its position was consumed at
/// request time). `T` is the accounting asset.
pub async fn build_settle_queued_request(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    treasury_id: ObjectID,
    global_seq: u64,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    let seq = pt.pure(&global_seq)?;
    vault_call(
        pt,
        refs.package,
        "settle_queued_request",
        refs.deposit_tag()?,
        vec![vault, cfg, treasury, seq],
    );
    Ok(())
}

/// `vault::claim_settlement_curator_fees<T>(vault, cap)` — the curator
/// pulls performance fees crystallized at settlement redemptions. `T` is
/// the accounting asset; the coin lands with the sender.
pub async fn build_claim_settlement_curator_fees(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &TradingVaultRefs<'_>,
    curator_cap: ObjectID,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(client, curator_cap).await?)?;
    vault_call(
        pt,
        refs.package,
        "claim_settlement_curator_fees",
        refs.deposit_tag()?,
        vec![vault, cap],
    );
    Ok(())
}

// ── curator provisioning (SO-345) ──────────────────────────────────────

/// `vault::create_vault<T>`'s config arguments. Order matters — it mirrors
/// the Move parameter list (v2 appends the immutable `CapitalStructure`
/// params + terms provenance). The `CuratorCap` always lands with the tx
/// sender: the creator IS the initial curator.
///
/// Untranched vault: `structure_code` 0 and ALL six tranche params 0
/// (the contract aborts otherwise); `structure_code` 1 = senior/junior.
/// `upside_code` selects the senior upside mode; `spec_hash` is the
/// content hash of the exact terms document `terms_version` names
/// (§9.2).
#[derive(Debug, Clone)]
pub struct CreateVaultSpec {
    pub lockup_ms: u64,
    pub curator_fee_bps: u64,
    pub unwind_grace_ms: u64,
    pub structure_code: u8,
    pub senior_hurdle_bps_annual: u64,
    pub target_junior_bps: u64,
    pub maintenance_junior_bps: u64,
    pub upside_code: u8,
    pub residual_participation_bps: u64,
    pub total_return_cap_bps: u64,
    pub terms_version: u64,
    pub spec_hash: Vec<u8>,
}

/// Tranche wire codes (`capital::tranche_from_code`).
pub const TRANCHE_UNTRANCHED: u8 = 0;
pub const TRANCHE_SENIOR: u8 = 1;
pub const TRANCHE_JUNIOR: u8 = 2;

/// The tranche a bot's own deposits land in: an untranched vault takes
/// only tranche 0 and a tranched vault rejects it (abort 121), where the
/// bot's capital is junior — the risk-bearing side its quoting budget is
/// measured against.
pub fn deposit_tranche_code(structure_code: u8) -> u8 {
    if structure_code == 0 { TRANCHE_UNTRANCHED } else { TRANCHE_JUNIOR }
}

/// Capital-structure terms (SO-418) as they appear under a bot's
/// `[..provision]` config table — `#[serde(flatten)]`ed into both bots'
/// provision configs so the keys and validation stay identical. Defaults
/// = UNTRANCHED; `structure_code` 1 requires all six tranche params set
/// coherently within the protocol floors or `create_vault` aborts.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrancheParams {
    /// 0 = untranched (default), 1 = senior/junior.
    pub structure_code: u8,
    pub senior_hurdle_bps_annual: u64,
    pub target_junior_bps: u64,
    pub maintenance_junior_bps: u64,
    pub upside_code: u8,
    pub residual_participation_bps: u64,
    pub total_return_cap_bps: u64,
    /// Terms-document version recorded immutably on the vault (§9.2).
    pub terms_version: u64,
    /// Hex content hash of the terms document `terms_version` names.
    /// Empty = no hash recorded.
    pub spec_hash: String,
}

impl Default for TrancheParams {
    fn default() -> Self {
        Self {
            structure_code: 0,
            senior_hurdle_bps_annual: 0,
            target_junior_bps: 0,
            maintenance_junior_bps: 0,
            upside_code: 0,
            residual_participation_bps: 0,
            total_return_cap_bps: 0,
            terms_version: 1,
            spec_hash: String::new(),
        }
    }
}

impl TrancheParams {
    /// The `CreateVaultSpec` these terms describe.
    pub fn create_vault_spec(
        &self,
        lockup_ms: u64,
        curator_fee_bps: u64,
        unwind_grace_ms: u64,
    ) -> Result<CreateVaultSpec> {
        let spec_hash = if self.spec_hash.is_empty() {
            Vec::new()
        } else {
            hex::decode(self.spec_hash.trim_start_matches("0x"))
                .map_err(|e| anyhow!("bad spec_hash: {e}"))?
        };
        Ok(CreateVaultSpec {
            lockup_ms,
            curator_fee_bps,
            unwind_grace_ms,
            structure_code: self.structure_code,
            senior_hurdle_bps_annual: self.senior_hurdle_bps_annual,
            target_junior_bps: self.target_junior_bps,
            maintenance_junior_bps: self.maintenance_junior_bps,
            upside_code: self.upside_code,
            residual_participation_bps: self.residual_participation_bps,
            total_return_cap_bps: self.total_return_cap_bps,
            terms_version: self.terms_version,
            spec_hash,
        })
    }
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
    whitelist_id: ObjectID,
    deposit_type: &str,
    spec: &CreateVaultSpec,
    gas_budget: u64,
) -> Result<VaultCreation> {
    info!(%package, deposit_type, "building create_vault PTB");
    let deposit_tag = TypeTag::from_str(deposit_type)
        .with_context(|| format!("parsing deposit type {deposit_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let cfg = pt.obj(shared_object_arg(client, protocol_config_id, false).await?)?;
    // Shared whitelist::Whitelist — the ingress gate (SO-383).
    let wl = pt.obj(shared_object_arg(client, whitelist_id, false).await?)?;
    let args = vec![
        cfg,
        wl,
        pt.pure(&spec.lockup_ms)?,
        pt.pure(&spec.curator_fee_bps)?,
        pt.pure(&spec.unwind_grace_ms)?,
        pt.pure(&spec.structure_code)?,
        pt.pure(&spec.senior_hurdle_bps_annual)?,
        pt.pure(&spec.target_junior_bps)?,
        pt.pure(&spec.maintenance_junior_bps)?,
        pt.pure(&spec.upside_code)?,
        pt.pure(&spec.residual_participation_bps)?,
        pt.pure(&spec.total_return_cap_bps)?,
        pt.pure(&spec.terms_version)?,
        pt.pure(&spec.spec_hash)?,
        clock_arg(&mut pt)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tranche_params_default_is_untranched() {
        let spec = TrancheParams::default().create_vault_spec(5, 10, 15).unwrap();
        assert_eq!(spec.structure_code, 0);
        assert_eq!((spec.lockup_ms, spec.curator_fee_bps, spec.unwind_grace_ms), (5, 10, 15));
        assert_eq!(spec.terms_version, 1);
        assert!(spec.spec_hash.is_empty());
    }

    #[test]
    fn spec_hash_parses_with_and_without_0x() {
        let mut p = TrancheParams { spec_hash: "0xdeadbeef".into(), ..Default::default() };
        assert_eq!(p.create_vault_spec(0, 0, 0).unwrap().spec_hash, vec![0xde, 0xad, 0xbe, 0xef]);
        p.spec_hash = "deadbeef".into();
        assert_eq!(p.create_vault_spec(0, 0, 0).unwrap().spec_hash, vec![0xde, 0xad, 0xbe, 0xef]);
        p.spec_hash = "not-hex".into();
        assert!(p.create_vault_spec(0, 0, 0).is_err());
    }

    #[test]
    fn deposit_tranche_is_junior_iff_tranched() {
        assert_eq!(deposit_tranche_code(0), TRANCHE_UNTRANCHED);
        assert_eq!(deposit_tranche_code(1), TRANCHE_JUNIOR);
    }
}
