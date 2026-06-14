//! PTB builders for the covered-call vault (`vault.move`) — the user
//! functions the frontend templates wrap and the permissionless cranks
//! the vault-keeper submits (vault-implementation-guide doc 05 §5).
//!
//! Oracle-gated cranks take the two Pyth `PriceInfoObject` ids; the
//! keeper prepends a Hermes price update to the same PTB (doc 04 §2) —
//! that helper lands with the keeper service.

use std::str::FromStr;

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command};
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::rfq::clock_arg;
use crate::tx::{owned_object_arg, shared_object_arg, submit_ptb};

/// Identity of one vault: its object id and the (U, S, VShare) triple its
/// generic calls are instantiated with.
pub struct VaultRefs<'a> {
    pub package: ObjectID,
    pub vault_id: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub share_type: &'a str,
}

impl VaultRefs<'_> {
    fn tags(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![
            TypeTag::from_str(self.underlying_type)
                .with_context(|| format!("parsing underlying type {}", self.underlying_type))?,
            TypeTag::from_str(self.settlement_type)
                .with_context(|| format!("parsing settlement type {}", self.settlement_type))?,
            TypeTag::from_str(self.share_type)
                .with_context(|| format!("parsing share type {}", self.share_type))?,
        ])
    }

    /// Tags for the bucket-generic calls: (U, S, VShare, Call).
    fn tags_with_call(&self, call_type: &str) -> Result<Vec<TypeTag>> {
        let mut tags = self.tags()?;
        tags.push(
            TypeTag::from_str(call_type)
                .with_context(|| format!("parsing call type {call_type}"))?,
        );
        Ok(tags)
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

fn transfer_to_sender(
    pt: &mut ProgrammableTransactionBuilder,
    signer: &Signer,
    out: Argument,
) -> Result<()> {
    let recipient = pt.pure(&signer.address)?;
    pt.command(Command::TransferObjects(vec![out], recipient));
    Ok(())
}

// ════════════════════════════ user functions ════════════════════════════

/// `vault::deposit`: split `amount` out of an owned underlying coin and
/// queue it; the `DepositReceipt` transfers to the signer.
pub async fn deposit(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    funding_coin: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, amount, "building vault::deposit PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, funding_coin).await?)?;
    let amount_arg = pt.pure(&amount)?;
    let payment = pt.command(Command::SplitCoins(funding, vec![amount_arg]));
    let receipt = vault_call(&mut pt, refs.package, "deposit", refs.tags()?, vec![vault, payment]);
    transfer_to_sender(&mut pt, signer, receipt)?;
    submit_ptb(client, signer, pt, gas_budget, "vault::deposit").await
}

/// `vault::claim_shares`: burn a deposit receipt for share coins.
pub async fn claim_shares(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    receipt: ObjectID,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %receipt, "building vault::claim_shares PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let receipt = pt.obj(owned_object_arg(client, receipt).await?)?;
    let shares = vault_call(&mut pt, refs.package, "claim_shares", refs.tags()?, vec![vault, receipt]);
    transfer_to_sender(&mut pt, signer, shares)?;
    submit_ptb(client, signer, pt, gas_budget, "vault::claim_shares").await
}

/// `vault::initiate_withdraw`: escrow share coins; the `WithdrawReceipt`
/// transfers to the signer.
pub async fn initiate_withdraw(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    share_coin: ObjectID,
    shares: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, shares, "building vault::initiate_withdraw PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, share_coin).await?)?;
    let shares_arg = pt.pure(&shares)?;
    let escrow = pt.command(Command::SplitCoins(funding, vec![shares_arg]));
    let receipt = vault_call(
        &mut pt,
        refs.package,
        "initiate_withdraw",
        refs.tags()?,
        vec![vault, escrow],
    );
    transfer_to_sender(&mut pt, signer, receipt)?;
    submit_ptb(client, signer, pt, gas_budget, "vault::initiate_withdraw").await
}

/// `vault::complete_withdraw`: pay out a finalized withdrawal.
pub async fn complete_withdraw(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    receipt: ObjectID,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %receipt, "building vault::complete_withdraw PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let receipt = pt.obj(owned_object_arg(client, receipt).await?)?;
    let payout = vault_call(
        &mut pt,
        refs.package,
        "complete_withdraw",
        refs.tags()?,
        vec![vault, receipt],
    );
    transfer_to_sender(&mut pt, signer, payout)?;
    submit_ptb(client, signer, pt, gas_budget, "vault::complete_withdraw").await
}

/// `vault::instant_withdraw_pending`: cancel a queued deposit whose round
/// hasn't started.
pub async fn instant_withdraw_pending(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    receipt: ObjectID,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %receipt, "building vault::instant_withdraw_pending PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let receipt = pt.obj(owned_object_arg(client, receipt).await?)?;
    let refund = vault_call(
        &mut pt,
        refs.package,
        "instant_withdraw_pending",
        refs.tags()?,
        vec![vault, receipt],
    );
    transfer_to_sender(&mut pt, signer, refund)?;
    submit_ptb(client, signer, pt, gas_budget, "vault::instant_withdraw_pending").await
}

// ════════════════════ lifecycle cranks (permissionless) ════════════════════

/// The two Pyth price objects an oracle-gated crank reads.
pub struct PriceInfoRefs {
    pub underlying_info: ObjectID,
    pub settlement_info: ObjectID,
}

impl PriceInfoRefs {
    async fn args(
        &self,
        client: &SuiClient,
        pt: &mut ProgrammableTransactionBuilder,
    ) -> Result<(Argument, Argument)> {
        let u = pt.obj(shared_object_arg(client, self.underlying_info, false).await?)?;
        let s = pt.obj(shared_object_arg(client, self.settlement_info, false).await?)?;
        Ok((u, s))
    }
}

/// `vault::crank_redeem`: redeem the next FIFO position (flips the phase
/// at expiry).
pub async fn crank_redeem(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    bucket_id: ObjectID,
    call_type: &str,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %bucket_id, "building vault::crank_redeem PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, bucket_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;
    vault_call(
        &mut pt,
        refs.package,
        "crank_redeem",
        refs.tags_with_call(call_type)?,
        vec![vault, bucket, clock],
    );
    submit_ptb(client, signer, pt, gas_budget, "vault::crank_redeem").await
}

/// Add the `vault::finalize_round` call to an in-progress PTB. The
/// keeper prepends a Pyth price update first (`tx::pyth_update`); the
/// shared-object inputs dedup.
pub async fn build_finalize_round(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &VaultRefs<'_>,
    treasury_id: ObjectID,
    prices: &PriceInfoRefs,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    let (u_info, s_info) = prices.args(client, pt).await?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "finalize_round",
        refs.tags()?,
        vec![vault, treasury, u_info, s_info, clock],
    );
    Ok(())
}

/// `vault::finalize_round`: lock pps, charge fees, process queues.
pub async fn finalize_round(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    treasury_id: ObjectID,
    prices: &PriceInfoRefs,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, "building vault::finalize_round PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    build_finalize_round(client, &mut pt, refs, treasury_id, prices).await?;
    submit_ptb(client, signer, pt, gas_budget, "vault::finalize_round").await
}

/// Add the `vault::select_bucket` call to an in-progress PTB.
pub async fn build_select_bucket(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &VaultRefs<'_>,
    bucket_id: ObjectID,
    call_type: &str,
    prices: &PriceInfoRefs,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, bucket_id, false).await?)?;
    let (u_info, s_info) = prices.args(client, pt).await?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "select_bucket",
        refs.tags_with_call(call_type)?,
        vec![vault, bucket, u_info, s_info, clock],
    );
    Ok(())
}

/// `vault::select_bucket`: pick the round's bucket (Pyth-banded).
pub async fn select_bucket(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    bucket_id: ObjectID,
    call_type: &str,
    prices: &PriceInfoRefs,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %bucket_id, "building vault::select_bucket PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    build_select_bucket(client, &mut pt, refs, bucket_id, call_type, prices).await?;
    submit_ptb(client, signer, pt, gas_budget, "vault::select_bucket").await
}

/// Add the `vault::open_rfq` call to an in-progress PTB.
pub async fn build_open_rfq(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &VaultRefs<'_>,
    bucket_id: ObjectID,
    call_type: &str,
    slice_amount: u64,
    prices: &PriceInfoRefs,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, bucket_id, false).await?)?;
    let slice = pt.pure(&slice_amount)?;
    let (u_info, s_info) = prices.args(client, pt).await?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "open_rfq",
        refs.tags_with_call(call_type)?,
        vec![vault, bucket, slice, u_info, s_info, clock],
    );
    Ok(())
}

/// `vault::open_rfq`: escrow a slice into a coupled auction.
#[allow(clippy::too_many_arguments)]
pub async fn open_rfq(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    bucket_id: ObjectID,
    call_type: &str,
    slice_amount: u64,
    prices: &PriceInfoRefs,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, slice_amount, "building vault::open_rfq PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    build_open_rfq(client, &mut pt, refs, bucket_id, call_type, slice_amount, prices).await?;
    submit_ptb(client, signer, pt, gas_budget, "vault::open_rfq").await
}

/// `vault::settle_rfq`: resolve a coupled auction in place.
#[allow(clippy::too_many_arguments)]
pub async fn settle_rfq(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    rfq_id: ObjectID,
    bucket_id: ObjectID,
    call_type: &str,
    protocol_config_id: ObjectID,
    treasury_id: ObjectID,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %rfq_id, "building vault::settle_rfq PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let rfq = pt.obj(shared_object_arg(client, rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, treasury_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;
    vault_call(
        &mut pt,
        refs.package,
        "settle_rfq",
        refs.tags_with_call(call_type)?,
        vec![vault, rfq, bucket, config, treasury, clock],
    );
    submit_ptb(client, signer, pt, gas_budget, "vault::settle_rfq").await
}

/// `vault::settle_rfq_expired`: recover a coupled auction whose bucket
/// died mid-auction.
pub async fn settle_rfq_expired(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    rfq_id: ObjectID,
    bucket_id: ObjectID,
    call_type: &str,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %rfq_id, "building vault::settle_rfq_expired PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let rfq = pt.obj(shared_object_arg(client, rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, bucket_id, false).await?)?;
    let clock = clock_arg(&mut pt)?;
    vault_call(
        &mut pt,
        refs.package,
        "settle_rfq_expired",
        refs.tags_with_call(call_type)?,
        vec![vault, rfq, bucket, clock],
    );
    submit_ptb(client, signer, pt, gas_budget, "vault::settle_rfq_expired").await
}

/// Add `vault::open_swap_rfq` to an in-progress PTB (the keeper prepends a
/// Pyth update). Escrows up to `amount_s` of the vault's settlement
/// proceeds into a coupled swap auction; MMs then bid underlying for it.
pub async fn build_open_swap_rfq(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &VaultRefs<'_>,
    amount_s: u64,
    prices: &PriceInfoRefs,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let amount = pt.pure(&amount_s)?;
    let (u_info, s_info) = prices.args(client, pt).await?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "open_swap_rfq",
        refs.tags()?,
        vec![vault, amount, u_info, s_info, clock],
    );
    Ok(())
}

/// `vault::open_swap_rfq`: open a proceeds-swap auction.
pub async fn open_swap_rfq(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    amount_s: u64,
    prices: &PriceInfoRefs,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, amount_s, "building vault::open_swap_rfq PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    build_open_swap_rfq(client, &mut pt, refs, amount_s, prices).await?;
    submit_ptb(client, signer, pt, gas_budget, "vault::open_swap_rfq").await
}

/// Add `vault::settle_swap_rfq` to an in-progress PTB. Resolves a coupled
/// swap auction: the winning bid is re-checked against the fresh Pyth
/// cross prepended to the same PTB.
pub async fn build_settle_swap_rfq(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &VaultRefs<'_>,
    swap_rfq_id: ObjectID,
    prices: &PriceInfoRefs,
) -> Result<()> {
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let auction = pt.obj(shared_object_arg(client, swap_rfq_id, true).await?)?;
    let (u_info, s_info) = prices.args(client, pt).await?;
    let clock = clock_arg(pt)?;
    vault_call(
        pt,
        refs.package,
        "settle_swap_rfq",
        refs.tags()?,
        vec![vault, auction, u_info, s_info, clock],
    );
    Ok(())
}

/// `vault::settle_swap_rfq`: resolve a proceeds-swap auction.
pub async fn settle_swap_rfq(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs<'_>,
    swap_rfq_id: ObjectID,
    prices: &PriceInfoRefs,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(vault = %refs.vault_id, %swap_rfq_id, "building vault::settle_swap_rfq PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    build_settle_swap_rfq(client, &mut pt, refs, swap_rfq_id, prices).await?;
    submit_ptb(client, signer, pt, gas_budget, "vault::settle_swap_rfq").await
}
