//! Pyth's half of the oracle seam (SO-335).
//!
//! Pyth prices live in shared `PriceInfoObject`s that must be REFRESHED
//! before they are read, so the prefix is four Move calls per PTB
//! (`crate::tx::pyth_update`) plus an update fee split off gas, and
//! `attest` then takes the two refreshed objects by reference.
//!
//! This module is a thin adapter over the existing `pyth_update`
//! machinery — the accumulator handling itself is unchanged and still
//! lives there, because it is Pyth protocol detail rather than seam
//! logic.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::pyth_update::{prepend_price_update, PythHandles};
use crate::tx::shared_object_arg;
use crate::chain::ChainClient;

/// What the Pyth provider needs to emit legs for one PTB.
#[derive(Debug, Clone)]
pub struct PythLegs<'a> {
    /// `oracle_pyth` package id (ours).
    pub adapter_pkg: ObjectID,
    /// The adapter's shared `PythFeedRegistry`.
    pub feed_registry_id: ObjectID,
    /// On-chain Pyth + Wormhole deployment.
    pub handles: &'a PythHandles,
    /// Binary accumulator payload from Hermes (`encoding=base64`,
    /// decoded). Only Pyth can sign this; a price cache cannot serve it.
    pub accumulator_update: &'a [u8],
    /// coin type → that feed's shared `PriceInfoObject`.
    pub price_infos: &'a BTreeMap<String, ObjectID>,
    /// Who signs (and pays for) the transaction, and with what budget. The
    /// per-feed update fee is funded to match how that wallet pays gas — see
    /// `pyth_update::prepend_price_update`.
    pub sender: SuiAddress,
    pub gas_budget: u64,
}

/// Object arguments produced by the prefix, ready for `attest`.
pub struct Prepared {
    pub deposit_info: Argument,
    infos: BTreeMap<String, Argument>,
}

impl Prepared {
    pub fn info_arg(&self, asset: &str) -> Result<Argument> {
        self.infos
            .get(asset)
            .copied()
            .ok_or_else(|| anyhow!("no PriceInfoObject argument prepared for {asset}"))
    }
}

/// Emit the accumulator prefix and resolve every object argument the
/// attestations will need.
///
/// The deposit asset is always refreshed even when nothing else needs
/// it: it is the quote leg of every cross, so a stale deposit feed would
/// fail the adapter's staleness check on assets that are themselves
/// fresh.
pub async fn prepare(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    legs: &PythLegs<'_>,
    assets: &[String],
    deposit_type: &str,
) -> Result<Prepared> {
    let deposit_info_id = *legs
        .price_infos
        .get(deposit_type)
        .ok_or_else(|| anyhow!("no PriceInfoObject for the deposit asset {deposit_type}"))?;

    let mut update_ids = vec![deposit_info_id];
    for t in assets {
        let info = *legs
            .price_infos
            .get(t)
            .ok_or_else(|| anyhow!("no PriceInfoObject for {t}"))?;
        if !update_ids.contains(&info) {
            update_ids.push(info);
        }
    }

    prepend_price_update(
        client,
        legs.sender,
        legs.gas_budget,
        pt,
        legs.handles,
        legs.accumulator_update,
        &update_ids,
    )
    .await
    .context("building pyth update prefix")?;

    let deposit_info = pt.obj(shared_object_arg(client, deposit_info_id, false).await?)?;
    let mut infos = BTreeMap::new();
    for t in assets {
        let id = legs.price_infos[t];
        let arg = pt.obj(shared_object_arg(client, id, false).await?)?;
        infos.insert(t.clone(), arg);
    }
    Ok(Prepared {
        deposit_info,
        infos,
    })
}
