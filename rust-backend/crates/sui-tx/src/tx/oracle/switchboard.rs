//! Switchboard's half of the oracle seam (SO-335).
//!
//! Structurally simpler than Pyth. There is no per-feed shared object to
//! refresh: one call to
//! `switchboard::quote_submit_action::run_{N}` validates N
//! oracles' signatures and returns a `Quotes` bundle as an in-PTB value,
//! and every `oracle_switchboard::attest` reads its own feed out of that
//! one bundle.
//!
//! Consequences worth naming, because they are why the switch is cheap:
//!
//! - **No update fee.** Nothing is written on chain, so there is no
//!   `update_fee_mist` to split off gas.
//! - **No shared-object writes.** Appraisals stop contending on the
//!   `PriceInfoObject`s, so they can execute in parallel.
//! - **One prefix command regardless of asset count**, versus Pyth's
//!   per-feed refresh.
//!
//! The payload itself (signed oracle responses) comes from Crossbar via
//! `switchboard-client`; this module only lays it into the PTB.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use move_core_types::identifier::Identifier;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::{clock_arg, shared_object_arg};
use crate::chain::ChainClient;

// The module name is `quote_submit_action` ON CHAIN (testnet 8th publish
// `0x0ea79f9c…`, checked via sui_getNormalizedMoveModule: run_1..run_6,
// identical signatures, returns `quote::Quotes`). The git checkout our
// adapter builds against has since RENAMED it `quote_submit_result_action`
// — target what is published, not what the branch head says, or every
// prefix call is FunctionNotFound (observed live, SO-346).

/// Largest `run_N` arity the Switchboard package exposes.
pub const MAX_ORACLES: usize = 6;

/// One Crossbar-sourced quote bundle, in the exact shape
/// `quote_submit_action::run_N` takes.
///
/// The vectors are parallel and per-feed; `signatures` and `oracle_ids`
/// are per-oracle. `run_N` is selected by `oracle_ids.len()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchboardQuotePayload {
    pub feed_ids: Vec<Vec<u8>>,
    pub values: Vec<u128>,
    pub values_neg: Vec<bool>,
    pub min_oracle_samples: Vec<u8>,
    pub signatures: Vec<Vec<u8>>,
    pub slot: u64,
    pub timestamp_seconds: u64,
    /// Signing oracles, in the order their signatures appear.
    pub oracle_ids: Vec<ObjectID>,
    /// The queue those oracles belong to. `attest` does not check this —
    /// `Quotes` carries the queue id and the Switchboard package
    /// validates membership when the bundle is built.
    pub queue_id: ObjectID,
}

impl Default for SwitchboardQuotePayload {
    fn default() -> Self {
        Self {
            feed_ids: Vec::new(),
            values: Vec::new(),
            values_neg: Vec::new(),
            min_oracle_samples: Vec::new(),
            signatures: Vec::new(),
            slot: 0,
            timestamp_seconds: 0,
            oracle_ids: Vec::new(),
            queue_id: ObjectID::ZERO,
        }
    }
}

impl SwitchboardQuotePayload {
    /// `run_1` … `run_6`, chosen by oracle count.
    pub fn run_function(&self) -> Result<String> {
        let n = self.oracle_ids.len();
        if n == 0 || n > MAX_ORACLES {
            return Err(anyhow!(
                "switchboard quote payload has {n} oracles; the package exposes run_1..run_{MAX_ORACLES}"
            ));
        }
        Ok(format!("run_{n}"))
    }

    /// Cheap shape check before we spend a PTB on it.
    pub fn validate(&self) -> Result<()> {
        self.run_function()?;
        if self.signatures.len() != self.oracle_ids.len() {
            return Err(anyhow!(
                "switchboard payload has {} signatures for {} oracles",
                self.signatures.len(),
                self.oracle_ids.len()
            ));
        }
        let n = self.feed_ids.len();
        if n == 0 {
            return Err(anyhow!("switchboard payload carries no feeds"));
        }
        if self.values.len() != n || self.values_neg.len() != n || self.min_oracle_samples.len() != n
        {
            return Err(anyhow!(
                "switchboard payload vectors disagree: {n} feed ids, {} values, {} signs, {} sample counts",
                self.values.len(),
                self.values_neg.len(),
                self.min_oracle_samples.len()
            ));
        }
        Ok(())
    }
}

/// What the Switchboard provider needs to emit legs for one PTB.
#[derive(Debug, Clone)]
pub struct SwitchboardLegs<'a> {
    /// `oracle_switchboard` package id (ours).
    pub adapter_pkg: ObjectID,
    /// The adapter's shared `SwitchboardFeedRegistry`.
    pub feed_registry_id: ObjectID,
    /// Switchboard's own `on_demand` package id.
    pub switchboard_pkg: ObjectID,
    pub payload: &'a SwitchboardQuotePayload,
    /// coin type → 32-byte feed hash. Coverage only; the values used
    /// on-chain come from the adapter's own registry, so a wrong hash
    /// here means "no leg", never "wrong price".
    pub feed_hashes: &'a BTreeMap<String, Vec<u8>>,
}

/// Emit the quote-submit call, returning the `Quotes` value every
/// `attest` in this PTB will read from.
pub async fn prepare(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    legs: &SwitchboardLegs<'_>,
) -> Result<Argument> {
    let p = legs.payload;
    p.validate()?;

    let feed_ids = pt.pure(p.feed_ids.clone())?;
    let values = pt.pure(p.values.clone())?;
    let values_neg = pt.pure(p.values_neg.clone())?;
    let min_samples = pt.pure(p.min_oracle_samples.clone())?;
    let signatures = pt.pure(p.signatures.clone())?;
    let slot = pt.pure(p.slot)?;
    let ts = pt.pure(p.timestamp_seconds)?;

    let mut args = vec![
        feed_ids,
        values,
        values_neg,
        min_samples,
        signatures,
        slot,
        ts,
    ];
    // Oracles are immutable refs, in signature order.
    for oracle_id in &p.oracle_ids {
        args.push(pt.obj(shared_object_arg(client, *oracle_id, false).await?)?);
    }
    args.push(pt.obj(shared_object_arg(client, p.queue_id, false).await?)?);
    args.push(clock_arg(pt)?);

    Ok(pt.programmable_move_call(
        legs.switchboard_pkg,
        Identifier::new("quote_submit_action").unwrap(),
        Identifier::new(p.run_function()?).unwrap(),
        vec![],
        args,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(oracles: usize, feeds: usize) -> SwitchboardQuotePayload {
        SwitchboardQuotePayload {
            feed_ids: vec![vec![1u8; 32]; feeds],
            values: vec![1u128; feeds],
            values_neg: vec![false; feeds],
            min_oracle_samples: vec![1u8; feeds],
            signatures: vec![vec![2u8; 64]; oracles],
            slot: 1,
            timestamp_seconds: 2,
            oracle_ids: vec![ObjectID::ZERO; oracles],
            queue_id: ObjectID::ZERO,
        }
    }

    #[test]
    fn run_function_tracks_oracle_count() {
        assert_eq!(payload(1, 1).run_function().unwrap(), "run_1");
        assert_eq!(payload(3, 2).run_function().unwrap(), "run_3");
        assert_eq!(payload(6, 1).run_function().unwrap(), "run_6");
    }

    #[test]
    fn arities_outside_run_1_to_6_are_rejected() {
        // Better to fail here than to build a PTB calling a function the
        // package does not export.
        assert!(payload(0, 1).run_function().is_err());
        assert!(payload(7, 1).run_function().is_err());
    }

    #[test]
    fn mismatched_signature_count_is_rejected() {
        let mut p = payload(3, 1);
        p.signatures.pop();
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("2 signatures for 3 oracles"), "{err}");
    }

    #[test]
    fn ragged_per_feed_vectors_are_rejected() {
        let mut p = payload(1, 3);
        p.values.pop();
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("disagree"), "{err}");
    }

    #[test]
    fn empty_payload_is_rejected() {
        let p = payload(1, 0);
        assert!(p.validate().unwrap_err().to_string().contains("no feeds"));
    }

    #[test]
    fn a_well_formed_payload_validates() {
        payload(3, 4).validate().unwrap();
    }
}
