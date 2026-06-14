//! `vault::create_vault` PTB builder.
//!
//! A vault needs a fresh per-vault share coin (`TreasuryCap<VShare>`, zero
//! supply) the same way a bucket needs its `Call` coin. The options-scheduler
//! publishes a one-module `vshare` coin package (see `option-scheduler`'s
//! `codegen::generate_share` + [`super::coin_pkg::publish_coin_package`]),
//! harvests the cap, then calls this builder.
//!
//! One PTB, two commands: `vault::new_config(...)` assembles the `VaultConfig`
//! from its plain fields, then `vault::create_vault<U, S, VShare>` consumes
//! the cap and shares the vault. The created `Vault` id is read back from the
//! tx's `ObjectChanges`.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::ObjectChange;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::{owned_object_arg, submit_ptb};

/// Plain `VaultConfig` fields, in `vault::new_config` argument order. The
/// per-asset oracle pins (`*_feed_id`, `*_decimals`) come from token-info; the
/// rest is the scheduler's vault policy template.
pub struct VaultConfigArgs {
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    pub min_reserve_premium_bps: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub rfq_duration_ms: u64,
    pub rfq_snipe_window_ms: u64,
    pub rfq_snipe_extension_ms: u64,
    pub rfq_max_extension_ms: u64,
    pub rfq_min_increment_bps: u64,
    pub hold_premium_in_settlement: bool,
    pub max_swap_slippage_bps: u64,
    pub underlying_feed_id: Vec<u8>,
    pub settlement_feed_id: Vec<u8>,
    pub max_price_age_secs: u64,
    pub max_conf_bps: u64,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
}

/// One vault to create: its asset triple, the harvested share cap, and config.
pub struct CreateVaultSpec {
    pub underlying_type: String,
    pub settlement_type: String,
    /// Canonical `VShare` type tag (`0x<pkg>::vshare::VSHARE`).
    pub share_type: String,
    /// The freshly-published `TreasuryCap<VShare>` object id (owned by signer).
    pub share_cap_id: ObjectID,
    pub config: VaultConfigArgs,
}

pub struct VaultCreation {
    pub digest: String,
    pub vault_id: ObjectID,
}

/// Build and submit `vault::new_config` + `vault::create_vault<U, S, VShare>`
/// in one PTB, returning the created vault id.
pub async fn create_vault(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    spec: &CreateVaultSpec,
    gas_budget: u64,
) -> Result<VaultCreation> {
    info!(
        %package,
        underlying = %spec.underlying_type,
        settlement = %spec.settlement_type,
        share = %spec.share_type,
        "building create_vault PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // AdminCap and the share TreasuryCap are both owned objects; the cap is
    // consumed by value inside create_vault.
    let admin_arg = pt.obj(owned_object_arg(client, admin_cap).await?)?;
    let cap_arg = pt.obj(owned_object_arg(client, spec.share_cap_id).await?)?;

    let u_tag = TypeTag::from_str(&spec.underlying_type)
        .with_context(|| format!("parsing underlying type {}", spec.underlying_type))?;
    let s_tag = TypeTag::from_str(&spec.settlement_type)
        .with_context(|| format!("parsing settlement type {}", spec.settlement_type))?;
    let v_tag = TypeTag::from_str(&spec.share_type)
        .with_context(|| format!("parsing share type {}", spec.share_type))?;

    let vault_module = Identifier::new("vault").unwrap();
    let new_config_fn = Identifier::new("new_config").unwrap();
    let create_fn = Identifier::new("create_vault").unwrap();

    let c = &spec.config;
    // Order MUST match vault::new_config's parameter list exactly.
    let config_args: Vec<Argument> = vec![
        pt.pure(&c.mgmt_fee_bps_annual)?,
        pt.pure(&c.perf_fee_bps)?,
        pt.pure(&c.round_ms)?,
        pt.pure(&c.selling_window_ms)?,
        pt.pure(&c.min_strike_bps_over_spot)?,
        pt.pure(&c.max_strike_bps_over_spot)?,
        pt.pure(&c.min_expiry_lead_ms)?,
        pt.pure(&c.max_expiry_lead_ms)?,
        pt.pure(&c.min_reserve_premium_bps)?,
        pt.pure(&c.max_slice_amount)?,
        pt.pure(&c.max_open_rfqs)?,
        pt.pure(&c.rfq_duration_ms)?,
        pt.pure(&c.rfq_snipe_window_ms)?,
        pt.pure(&c.rfq_snipe_extension_ms)?,
        pt.pure(&c.rfq_max_extension_ms)?,
        pt.pure(&c.rfq_min_increment_bps)?,
        pt.pure(&c.hold_premium_in_settlement)?,
        pt.pure(&c.max_swap_slippage_bps)?,
        pt.pure(&c.underlying_feed_id)?,
        pt.pure(&c.settlement_feed_id)?,
        pt.pure(&c.max_price_age_secs)?,
        pt.pure(&c.max_conf_bps)?,
        pt.pure(&c.underlying_decimals)?,
        pt.pure(&c.settlement_decimals)?,
    ];
    let config = pt.programmable_move_call(
        package,
        vault_module.clone(),
        new_config_fn,
        vec![],
        config_args,
    );

    pt.programmable_move_call(
        package,
        vault_module,
        create_fn,
        vec![u_tag, s_tag, v_tag],
        vec![admin_arg, cap_arg, config],
    );

    let resp = submit_ptb(client, signer, pt, gas_budget, "vault::create_vault").await?;
    let vault_id = extract_vault_id(resp.object_changes.as_deref().unwrap_or(&[]))
        .ok_or_else(|| anyhow!("create_vault succeeded but no Vault object in ObjectChanges"))?;
    Ok(VaultCreation {
        digest: resp.digest.to_string(),
        vault_id,
    })
}

/// Pull the created `vault::Vault` ObjectID out of a tx's ObjectChanges.
fn extract_vault_id(changes: &[ObjectChange]) -> Option<ObjectID> {
    changes.iter().find_map(|c| match c {
        ObjectChange::Created {
            object_id,
            object_type,
            ..
        } if object_type.module.as_str() == "vault" && object_type.name.as_str() == "Vault" => {
            Some(*object_id)
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use move_core_types::account_address::AccountAddress;
    use move_core_types::language_storage::StructTag;
    use sui_types::base_types::{ObjectDigest, SequenceNumber, SuiAddress};
    use sui_types::object::Owner;

    fn created(id: ObjectID, module: &str, name: &str) -> ObjectChange {
        ObjectChange::Created {
            sender: SuiAddress::ZERO,
            owner: Owner::Shared {
                initial_shared_version: SequenceNumber::from_u64(1),
            },
            object_type: StructTag {
                address: AccountAddress::from_hex_literal("0xabc").unwrap(),
                module: Identifier::new(module).unwrap(),
                name: Identifier::new(name).unwrap(),
                type_params: vec![],
            },
            object_id: id,
            version: SequenceNumber::from_u64(1),
            digest: ObjectDigest::random(),
        }
    }

    #[test]
    fn finds_vault_among_other_creates() {
        let vault = ObjectID::random();
        let changes = vec![
            created(ObjectID::random(), "coin", "TreasuryCap"),
            created(vault, "vault", "Vault"),
            created(ObjectID::random(), "vault", "DepositReceipt"),
        ];
        assert_eq!(extract_vault_id(&changes), Some(vault));
    }

    #[test]
    fn none_when_no_vault_created() {
        let changes = vec![created(ObjectID::random(), "coin", "TreasuryCap")];
        assert_eq!(extract_vault_id(&changes), None);
    }
}
