//! Bucket-roll executor.
//!
//! Builds a `NewCallOptionArgs`, submits it via the shared admin tx
//! builder, and pulls every created bucket id out of `ObjectChanges` so
//! we can log them. Honors `--dry-run`: we log the args we *would* have
//! sent and return without touching the chain.

use anyhow::{Context, Result};
use sui_json_rpc_types::ObjectChange;
use sui_types::base_types::ObjectID;
use tracing::{debug, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::admin::{new_call_option, NewCallOptionArgs};

use crate::strike_grid::StrikeGrid;

#[derive(Debug, Clone)]
pub struct RollPlan {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_type: String,
    pub settlement_type: String,
    pub expiry_ms: u64,
    pub grid: StrikeGrid,
}

impl RollPlan {
    pub fn log_intent(&self, dry_run: bool) {
        info!(
            pair = %format!("{}/{}", self.underlying_symbol, self.settlement_symbol),
            expiry_ms = self.expiry_ms,
            start_strike = %self.grid.start_strike,
            strike_interval = %self.grid.strike_interval,
            count = self.grid.count,
            strike_scale = self.grid.strike_scale,
            dry_run,
            "rolling new bucket family"
        );
    }
}

pub struct RollOutcome {
    pub digest: String,
    pub bucket_ids: Vec<ObjectID>,
}

pub async fn submit(
    wrap: &SuiClientWrapper,
    package: ObjectID,
    admin_cap: ObjectID,
    plan: &RollPlan,
    gas_budget: u64,
) -> Result<RollOutcome> {
    debug!(
        %package,
        %admin_cap,
        underlying = %plan.underlying_type,
        settlement = %plan.settlement_type,
        expiry_ms = plan.expiry_ms,
        gas_budget,
        "submitting roll"
    );
    let resp = new_call_option(
        &wrap.client,
        &wrap.signer,
        &NewCallOptionArgs {
            package,
            admin_cap,
            underlying_type: &plan.underlying_type,
            settlement_type: &plan.settlement_type,
            expiry_ms: plan.expiry_ms,
            start_strike: plan.grid.start_strike,
            strike_interval: plan.grid.strike_interval,
            count: plan.grid.count,
            strike_scale: plan.grid.strike_scale,
        },
        gas_budget,
    )
    .await
    .context("submitting new_call_option")?;

    let digest = resp.digest.to_string();
    let bucket_ids = extract_bucket_ids(resp.object_changes.as_deref().unwrap_or(&[]));
    if bucket_ids.is_empty() {
        warn!(
            digest,
            "new_call_option succeeded but no Bucket objects observed in ObjectChanges — \
             relying on indexer to fill in"
        );
    }
    info!(
        digest,
        bucket_count = bucket_ids.len(),
        "roll submitted"
    );
    Ok(RollOutcome { digest, bucket_ids })
}

/// Pull `bucket::Bucket` ObjectIDs out of a tx's ObjectChanges, in the
/// order they appear. The chain emits one Created per strike for a
/// successful `new_call_option`, so the result lines up with the strike
/// grid the planner submitted.
pub(crate) fn extract_bucket_ids(changes: &[ObjectChange]) -> Vec<ObjectID> {
    debug!("extracting bucket ids from object changes");
    changes
        .iter()
        .filter_map(|c| match c {
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "bucket"
                && object_type.name.as_str() == "Bucket" =>
            {
                Some(*object_id)
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use move_core_types::language_storage::StructTag;
    use move_core_types::{account_address::AccountAddress, identifier::Identifier};
    use sui_types::base_types::{ObjectDigest, SequenceNumber, SuiAddress};
    use sui_types::object::Owner;

    // Sentinel package address used in every constructed StructTag. The
    // value doesn't matter — `extract_bucket_ids` only looks at the
    // module and type name.
    fn pkg() -> AccountAddress {
        AccountAddress::from_hex_literal("0xabc").unwrap()
    }

    fn struct_tag(module: &str, name: &str) -> StructTag {
        StructTag {
            address: pkg(),
            module: Identifier::new(module).unwrap(),
            name: Identifier::new(name).unwrap(),
            type_params: vec![],
        }
    }

    fn created(id: ObjectID, module: &str, name: &str) -> ObjectChange {
        ObjectChange::Created {
            sender: SuiAddress::ZERO,
            owner: Owner::Shared {
                initial_shared_version: SequenceNumber::from_u64(1),
            },
            object_type: struct_tag(module, name),
            object_id: id,
            version: SequenceNumber::from_u64(1),
            digest: ObjectDigest::random(),
        }
    }

    fn mutated(id: ObjectID, module: &str, name: &str) -> ObjectChange {
        ObjectChange::Mutated {
            sender: SuiAddress::ZERO,
            owner: Owner::AddressOwner(SuiAddress::ZERO),
            object_type: struct_tag(module, name),
            object_id: id,
            version: SequenceNumber::from_u64(2),
            previous_version: SequenceNumber::from_u64(1),
            digest: ObjectDigest::random(),
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(extract_bucket_ids(&[]).is_empty());
    }

    #[test]
    fn pulls_bucket_created_in_order() {
        // A typical new_call_option tx: one TreasuryCap created per
        // strike, one Bucket created per strike, plus the usual gas /
        // mutated bits. The function must return only the Bucket
        // Createds, in the order they appear.
        let b1 = ObjectID::random();
        let b2 = ObjectID::random();
        let b3 = ObjectID::random();
        let cap = ObjectID::random();
        let changes = vec![
            created(cap, "coin", "TreasuryCap"),
            created(b1, "bucket", "Bucket"),
            created(cap, "coin", "TreasuryCap"),
            created(b2, "bucket", "Bucket"),
            created(cap, "coin", "TreasuryCap"),
            created(b3, "bucket", "Bucket"),
        ];
        assert_eq!(extract_bucket_ids(&changes), vec![b1, b2, b3]);
    }

    #[test]
    fn ignores_mutated_buckets() {
        // A subsequent execute_write mutates an existing Bucket; the
        // roller must never confuse that for a freshly-rolled one.
        let b = ObjectID::random();
        let changes = vec![mutated(b, "bucket", "Bucket")];
        assert!(extract_bucket_ids(&changes).is_empty());
    }

    #[test]
    fn ignores_other_modules_and_types() {
        // Same module, wrong type name (PositionNFT, CallOptionToken) and
        // wrong module entirely (account::Account, sui::SUI) all get
        // dropped.
        let changes = vec![
            created(ObjectID::random(), "bucket", "PositionNFT"),
            created(ObjectID::random(), "account", "Account"),
            created(ObjectID::random(), "sui", "SUI"),
            created(ObjectID::random(), "BUCKET", "Bucket"), // case-sensitive
            created(ObjectID::random(), "bucket", "bucket"), // case-sensitive
        ];
        assert!(extract_bucket_ids(&changes).is_empty());
    }

    #[test]
    fn returns_only_bucket_among_mixed() {
        // The one true Bucket survives, everything else is filtered.
        let bucket_id = ObjectID::random();
        let changes = vec![
            created(ObjectID::random(), "coin", "TreasuryCap"),
            created(bucket_id, "bucket", "Bucket"),
            created(ObjectID::random(), "bucket", "PositionNFT"),
            mutated(ObjectID::random(), "bucket", "Bucket"),
        ];
        assert_eq!(extract_bucket_ids(&changes), vec![bucket_id]);
    }
}
