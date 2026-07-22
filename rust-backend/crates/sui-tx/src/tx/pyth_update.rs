//! Hermes → on-chain Pyth price update, as a PTB *prefix* (keeper
//! README §7).
//!
//! The vault's oracle-gated cranks (`select_bucket`, `open_rfq`,
//! `swap_proceeds`, `finalize_round`) read two shared
//! `PriceInfoObject`s and enforce `max_price_age_secs`, so the caller
//! must refresh those objects **in the same PTB** as the crank. The
//! standard Pyth Sui accumulator flow is:
//!
//! 1. off-chain: pull the embedded wormhole VAA out of the Hermes
//!    accumulator message ([`extract_vaa_from_accumulator`]),
//! 2. `wormhole::vaa::parse_and_verify(wormhole_state, vaa, clock)`,
//! 3. `pyth::pyth::create_authenticated_price_infos_using_accumulator(
//!    pyth_state, accumulator_message, verified_vaa, clock)`,
//! 4. one `pyth::pyth::update_single_price_feed(pyth_state, potato,
//!    price_info_object, fee, clock)` per feed (fee split from gas; the
//!    call matches updates to objects by feed id, order is irrelevant),
//! 5. `pyth::hot_potato_vector::destroy<PriceInfo>(potato)`.
//!
//! The shared-object inputs dedup with the crank's: the builder merges a
//! mutable (update) and immutable (crank read) reference to the same
//! `PriceInfoObject` into one mutable input.

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::{StructTag, TypeTag};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command};

use crate::tx::{clock_arg, shared_object_arg};

/// On-chain handles for the Pyth + Wormhole deployments on the target
/// network. Package ids are the *latest* (upgraded) packages the entry
/// calls target; state ids never change.
#[derive(Debug, Clone)]
pub struct PythHandles {
    pub pyth_package: ObjectID,
    pub wormhole_package: ObjectID,
    pub pyth_state_id: ObjectID,
    pub wormhole_state_id: ObjectID,
    /// Base update fee per feed in MIST (`state::get_base_update_fee`;
    /// 1 on mainnet/testnet today). Split from gas per
    /// `update_single_price_feed`.
    pub update_fee_mist: u64,
    /// The state's `b"price_info"` feed→PriceInfoObject table, pinned in
    /// config for RPC providers whose dynamic-field index is broken
    /// (publicnode). `None` → resolve via the dynamic field. The id
    /// never changes for a given Pyth deployment.
    pub price_info_table_id: Option<ObjectID>,
}

/// Accumulator-update magic: `"PNAU"`.
const ACCUMULATOR_MAGIC: &[u8; 4] = b"PNAU";
/// Update-type tag for Wormhole-Merkle proofs — the only kind Hermes
/// emits and the only kind the Sui contract accepts.
const PROOF_TYPE_WORMHOLE_MERKLE: u8 = 0;

/// Pull the embedded Wormhole VAA out of a Hermes accumulator message
/// (`binary.data[i]`, base64-decoded). Wire layout, per Pyth's
/// price-service SDK:
///
/// ```text
/// magic "PNAU" | major u8 | minor u8 | trailer_len u8 | trailer …
/// | proof_type u8 (0 = wormhole merkle) | vaa_len u16be | vaa …
/// ```
pub fn extract_vaa_from_accumulator(update: &[u8]) -> Result<Vec<u8>> {
    let need = |n: usize| -> Result<()> {
        if update.len() < n {
            Err(anyhow!(
                "accumulator update truncated: need {n} bytes, have {}",
                update.len()
            ))
        } else {
            Ok(())
        }
    };
    need(8)?;
    if &update[0..4] != ACCUMULATOR_MAGIC {
        return Err(anyhow!(
            "not an accumulator update: magic {:02x?}",
            &update[0..4]
        ));
    }
    let major = update[4];
    if major != 1 {
        return Err(anyhow!("unsupported accumulator major version {major}"));
    }
    let trailer_len = update[6] as usize;
    let mut off = 7 + trailer_len;
    need(off + 3)?;
    let proof_type = update[off];
    if proof_type != PROOF_TYPE_WORMHOLE_MERKLE {
        return Err(anyhow!("unsupported accumulator proof type {proof_type}"));
    }
    off += 1;
    let vaa_len = u16::from_be_bytes([update[off], update[off + 1]]) as usize;
    off += 2;
    need(off + vaa_len)?;
    Ok(update[off..off + vaa_len].to_vec())
}

/// Append the price-update calls for `accumulator_update` (one Hermes
/// `binary.data` entry covering every feed in `price_info_objects`) to
/// `pt`. Call this *before* adding the oracle-gated crank.
pub async fn prepend_price_update(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    handles: &PythHandles,
    accumulator_update: &[u8],
    price_info_objects: &[ObjectID],
) -> Result<()> {
    if price_info_objects.is_empty() {
        return Err(anyhow!("no price info objects to update"));
    }
    let vaa_bytes =
        extract_vaa_from_accumulator(accumulator_update).context("extracting embedded VAA")?;

    let wormhole_state =
        pt.obj(shared_object_arg(client, handles.wormhole_state_id, false).await?)?;
    let pyth_state = pt.obj(shared_object_arg(client, handles.pyth_state_id, false).await?)?;
    let clock = clock_arg(pt)?;

    let vaa_arg = pt.pure(&vaa_bytes)?;
    let verified_vaa = pt.programmable_move_call(
        handles.wormhole_package,
        Identifier::new("vaa").unwrap(),
        Identifier::new("parse_and_verify").unwrap(),
        vec![],
        vec![wormhole_state, vaa_arg, clock],
    );

    let message_arg = pt.pure(&accumulator_update.to_vec())?;
    let mut potato = pt.programmable_move_call(
        handles.pyth_package,
        Identifier::new("pyth").unwrap(),
        Identifier::new("create_authenticated_price_infos_using_accumulator").unwrap(),
        vec![],
        vec![pyth_state, message_arg, verified_vaa, clock],
    );

    for info_id in price_info_objects {
        let info = pt.obj(shared_object_arg(client, *info_id, true).await?)?;
        let fee_amount = pt.pure(&handles.update_fee_mist)?;
        let fee = pt.command(Command::SplitCoins(Argument::GasCoin, vec![fee_amount]));
        potato = pt.programmable_move_call(
            handles.pyth_package,
            Identifier::new("pyth").unwrap(),
            Identifier::new("update_single_price_feed").unwrap(),
            vec![],
            vec![pyth_state, potato, info, fee, clock],
        );
    }

    let price_info_tag = TypeTag::Struct(Box::new(StructTag {
        address: handles.pyth_package.into(),
        module: Identifier::new("price_info").unwrap(),
        name: Identifier::new("PriceInfo").unwrap(),
        type_params: vec![],
    }));
    pt.programmable_move_call(
        handles.pyth_package,
        Identifier::new("hot_potato_vector").unwrap(),
        Identifier::new("destroy").unwrap(),
        vec![price_info_tag],
        vec![potato],
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal well-formed accumulator update around `vaa`.
    fn accumulator(trailer: &[u8], vaa: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(ACCUMULATOR_MAGIC);
        buf.push(1); // major
        buf.push(0); // minor
        buf.push(trailer.len() as u8);
        buf.extend_from_slice(trailer);
        buf.push(PROOF_TYPE_WORMHOLE_MERKLE);
        buf.extend_from_slice(&(vaa.len() as u16).to_be_bytes());
        buf.extend_from_slice(vaa);
        // Merkle price updates follow in the real payload; the parser
        // must not care.
        buf.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        buf
    }

    #[test]
    fn extracts_vaa_with_and_without_trailer() {
        let vaa: Vec<u8> = (0u8..200).collect();
        assert_eq!(extract_vaa_from_accumulator(&accumulator(&[], &vaa)).unwrap(), vaa);
        assert_eq!(
            extract_vaa_from_accumulator(&accumulator(&[7, 7, 7], &vaa)).unwrap(),
            vaa
        );
    }

    #[test]
    fn rejects_wrong_magic_version_and_proof_type() {
        let vaa = vec![1u8; 16];
        let mut bad_magic = accumulator(&[], &vaa);
        bad_magic[0] = b'X';
        assert!(extract_vaa_from_accumulator(&bad_magic).is_err());

        let mut bad_major = accumulator(&[], &vaa);
        bad_major[4] = 2;
        assert!(extract_vaa_from_accumulator(&bad_major).is_err());

        let mut bad_proof = accumulator(&[], &vaa);
        bad_proof[7] = 1;
        assert!(extract_vaa_from_accumulator(&bad_proof).is_err());
    }

    #[test]
    fn rejects_truncated_payloads() {
        let vaa = vec![1u8; 64];
        let full = accumulator(&[], &vaa);
        // Cut inside the VAA body (header is 10 bytes here).
        assert!(extract_vaa_from_accumulator(&full[..20]).is_err());
        assert!(extract_vaa_from_accumulator(&full[..6]).is_err());
    }
}
