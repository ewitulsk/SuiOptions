//! Provider-agnostic price legs for a PTB (SO-335).
//!
//! Everything that needs an on-chain price — the appraisal composer here,
//! and its TypeScript twin in the browser — used to name `oracle_pyth`
//! directly and hand-roll Pyth's four-call accumulator prefix. That made
//! the oracle a compile-time choice.
//!
//! This module is the seam. A caller supplies [`OracleLegs`] (whatever
//! the *current* provider needs, resolved upstream from
//! `oracle-service`'s descriptor) and calls [`emit_price_legs`], which
//! emits the provider's prefix and one `attest` per asset, returning the
//! resulting `PriceAttestation` arguments keyed by coin type. Nothing
//! downstream knows which provider ran.
//!
//! ## Why an enum and not a trait
//!
//! The provider set is deliberately closed (`protocol_types::
//! OracleProvider` is not `non_exhaustive`), the two shapes differ enough
//! that a common trait would be mostly associated types, and the prefix
//! step is async — `async fn` in a trait object is still friction that
//! buys nothing here. Adding a provider means adding a variant and
//! fixing the resulting non-exhaustive match errors, which is exactly the
//! review we want.
//!
//! ## The two shapes
//!
//! | | Pyth | Switchboard |
//! |---|---|---|
//! | Prefix | 4 calls that REFRESH shared `PriceInfoObject`s | 1 call producing an in-PTB `Quotes` value |
//! | `attest` args | two `&PriceInfoObject` | one `&Quotes` |
//! | Update fee | yes (split from gas) | none |
//! | Shared-object writes | yes, per feed | none |

pub mod pyth;
pub mod switchboard;

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use protocol_types::OracleProvider;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::shared_object_arg;
use crate::chain::ChainClient;

pub use pyth::PythLegs;
pub use switchboard::{SwitchboardLegs, SwitchboardQuotePayload};

/// The live provider's inputs for one PTB.
#[derive(Debug, Clone)]
pub enum OracleLegs<'a> {
    Pyth(PythLegs<'a>),
    Switchboard(SwitchboardLegs<'a>),
}

impl OracleLegs<'_> {
    pub fn provider(&self) -> OracleProvider {
        match self {
            OracleLegs::Pyth(_) => OracleProvider::Pyth,
            OracleLegs::Switchboard(_) => OracleProvider::Switchboard,
        }
    }

    /// The adapter package whose `attest` this provider calls.
    pub fn adapter_pkg(&self) -> ObjectID {
        match self {
            OracleLegs::Pyth(l) => l.adapter_pkg,
            OracleLegs::Switchboard(l) => l.adapter_pkg,
        }
    }

    /// The adapter's own feed registry. Lives on the legs rather than in
    /// the caller's refs so a provider switch cannot leave a caller
    /// passing Pyth's registry to Switchboard's `attest`.
    pub fn feed_registry_id(&self) -> ObjectID {
        match self {
            OracleLegs::Pyth(l) => l.feed_registry_id,
            OracleLegs::Switchboard(l) => l.feed_registry_id,
        }
    }

    /// Can this provider price `asset` in this transaction? False means
    /// the caller must pass `option::none` for that leg and let the
    /// on-chain checks decide — an unpriced component that is actually
    /// zero is fine, a nonzero one correctly wedges the appraisal.
    pub fn can_price(&self, asset: &str) -> bool {
        match self {
            OracleLegs::Pyth(l) => l.price_infos.contains_key(asset),
            OracleLegs::Switchboard(l) => l.feed_hashes.contains_key(asset),
        }
    }

    /// Assets from `wanted` this provider can actually price, in order.
    pub fn attestable(&self, wanted: &[String]) -> Vec<String> {
        wanted.iter().filter(|a| self.can_price(a)).cloned().collect()
    }
}

/// Shared object ids every provider's `attest` needs that are NOT
/// provider-specific. The feed registry deliberately is not here — see
/// [`OracleLegs::feed_registry_id`].
#[derive(Debug, Clone, Copy)]
pub struct OracleRefs {
    /// `trading_vault::registry::OracleRegistry`.
    pub oracle_registry_id: ObjectID,
}

/// Emit the provider's prefix plus one `attest` per attestable asset.
///
/// Returns `coin type -> PriceAttestation` arguments. `assets` should
/// already be filtered with [`OracleLegs::attestable`]; anything in it
/// the provider cannot price is an error rather than a silent skip,
/// because the caller has to decide between "pass none" and "refuse" and
/// that decision does not belong here.
pub async fn emit_price_legs(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    legs: &OracleLegs<'_>,
    refs: &OracleRefs,
    assets: &[String],
    deposit_type: &str,
    clock: Argument,
) -> Result<BTreeMap<String, Argument>> {
    let mut out = BTreeMap::new();
    if assets.is_empty() {
        return Ok(out);
    }
    for a in assets {
        if !legs.can_price(a) {
            return Err(anyhow!(
                "{} cannot price {a} in this transaction — filter with \
                 OracleLegs::attestable before calling",
                legs.provider()
            ));
        }
    }

    let deposit_tag = TypeTag::from_str(deposit_type).context("parsing deposit type")?;
    let oracle_reg = pt.obj(shared_object_arg(client, refs.oracle_registry_id, false).await?)?;
    let feed_reg = pt.obj(shared_object_arg(client, legs.feed_registry_id(), false).await?)?;

    match legs {
        OracleLegs::Pyth(l) => {
            let prepared = pyth::prepare(client, pt, l, assets, deposit_type).await?;
            for asset in assets {
                let asset_tag = TypeTag::from_str(asset).context("parsing asset type")?;
                let att = pt.programmable_move_call(
                    l.adapter_pkg,
                    Identifier::new(OracleProvider::Pyth.adapter_module()).unwrap(),
                    Identifier::new("attest").unwrap(),
                    vec![asset_tag, deposit_tag.clone()],
                    vec![
                        feed_reg,
                        oracle_reg,
                        prepared.info_arg(asset)?,
                        prepared.deposit_info,
                        clock,
                    ],
                );
                out.insert(asset.clone(), att);
            }
        }
        OracleLegs::Switchboard(l) => {
            // One call yields the whole bundle; every attest reads its
            // own feed out of it, so N assets cost one prefix command
            // rather than N shared-object refreshes.
            let quotes = switchboard::prepare(client, pt, l).await?;
            for asset in assets {
                let asset_tag = TypeTag::from_str(asset).context("parsing asset type")?;
                let att = pt.programmable_move_call(
                    l.adapter_pkg,
                    Identifier::new(OracleProvider::Switchboard.adapter_module()).unwrap(),
                    Identifier::new("attest").unwrap(),
                    vec![asset_tag, deposit_tag.clone()],
                    vec![feed_reg, oracle_reg, quotes, clock],
                );
                out.insert(asset.clone(), att);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pyth_legs<'a>(
        infos: &'a BTreeMap<String, ObjectID>,
        handles: &'a crate::tx::pyth_update::PythHandles,
        update: &'a [u8],
    ) -> OracleLegs<'a> {
        OracleLegs::Pyth(PythLegs {
            adapter_pkg: ObjectID::ZERO,
            feed_registry_id: ObjectID::ZERO,
            handles,
            accumulator_update: update,
            price_infos: infos,
        })
    }

    #[test]
    fn attestable_filters_to_what_the_provider_covers() {
        let handles = crate::tx::pyth_update::PythHandles {
            pyth_package: ObjectID::ZERO,
            wormhole_package: ObjectID::ZERO,
            pyth_state_id: ObjectID::ZERO,
            wormhole_state_id: ObjectID::ZERO,
            update_fee_mist: 1,
            price_info_table_id: None,
        };
        let mut infos = BTreeMap::new();
        infos.insert("0x1::a::A".to_string(), ObjectID::ZERO);
        let update: Vec<u8> = vec![];
        let legs = pyth_legs(&infos, &handles, &update);

        let wanted = vec!["0x1::a::A".to_string(), "0x1::b::B".to_string()];
        assert_eq!(legs.attestable(&wanted), vec!["0x1::a::A".to_string()]);
        assert!(legs.can_price("0x1::a::A"));
        assert!(!legs.can_price("0x1::b::B"));
        assert_eq!(legs.provider(), OracleProvider::Pyth);
    }

    #[test]
    fn switchboard_coverage_comes_from_feed_hashes() {
        let mut hashes = BTreeMap::new();
        hashes.insert("0x1::a::A".to_string(), vec![7u8; 32]);
        let payload = SwitchboardQuotePayload::default();
        let legs = OracleLegs::Switchboard(SwitchboardLegs {
            adapter_pkg: ObjectID::ZERO,
            feed_registry_id: ObjectID::ZERO,
            switchboard_pkg: ObjectID::ZERO,
            payload: &payload,
            feed_hashes: &hashes,
        });
        assert!(legs.can_price("0x1::a::A"));
        assert!(!legs.can_price("0x1::b::B"));
        assert_eq!(legs.provider(), OracleProvider::Switchboard);
        // The provider is carried in the value, not in a config flag the
        // caller has to re-read — that is what makes the switch total.
        assert_eq!(legs.provider().adapter_module(), "oracle_switchboard");
    }
}
