//! Bucket-roll executor.
//!
//! A roll now publishes a fresh option-coin package and creates one bucket per
//! strike, each backed by its own fungible `Coin<Call>`:
//!
//!   1. codegen N One-Time-Witness coin modules for the strike grid,
//!   2. compile the package in-process (`sui-move-build`),
//!   3. publish it and harvest the `TreasuryCap`s its inits minted,
//!   4. `bucket::create_bucket<U, S, Call_i>` once per cap in a single PTB,
//!   5. pull every created bucket id out of `ObjectChanges` so we can log them.
//!
//! Honors `--dry-run`: the caller logs the args it *would* have sent and never
//! reaches `submit`.

use anyhow::{Context, Result};
use sui_json_rpc_types::ObjectChange;
use sui_move_build::BuildConfig;
use sui_types::base_types::ObjectID;
use tracing::{debug, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::coin_pkg::{self, CreateBucketSpec};

use crate::codegen;
use crate::strike_grid::StrikeGrid;

#[derive(Debug, Clone)]
pub struct RollPlan {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_type: String,
    pub settlement_type: String,
    /// Underlying decimals — the option coin mints with the same decimals so
    /// one option smallest-unit == one underlying smallest-unit.
    pub underlying_decimals: u8,
    /// Settlement decimals — the DeepBook pool's quote side. Paired with the
    /// call coin (base, which inherits `underlying_decimals`) to derive the
    /// pool's tick/lot/min grid.
    pub settlement_decimals: u8,
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
    /// The Call coin types created this roll (one per strike). The base side
    /// of each bucket's DeepBook pool — created best-effort after the buckets
    /// land (see `main::create_pools_for_roll`).
    pub call_types: Vec<String>,
}

/// Classification of a submit error for the local-rolls retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Build, sign, preflight, or gas-stale: the tx never reached
    /// consensus. Safe to delete the pending row and retry on next tick.
    DefinitelyNotSent,
    /// RPC timeout, transport error after transmit, unknown: the tx may
    /// or may not have been accepted. Must go to `needs_reconciliation`.
    Ambiguous,
}

/// Inspect an `anyhow::Error` from `submit` and decide whether the tx
/// definitely never reached consensus or might have.
pub fn classify_error(err: &anyhow::Error) -> ErrorClass {
    let msg = format!("{err:#}").to_lowercase();
    // Build/sign/preflight errors — the tx was never transmitted.
    let definitely_not_sent = [
        "building",
        "parsing type tag",
        "signature",
        "gas price",
        "gas budget",
        "insufficient gas",
        "not enough coins",
        "reverted",
        "object not found",
        "invalid object",
        "building bucket::new_call_option tx",
    ];
    for pat in &definitely_not_sent {
        if msg.contains(pat) {
            return ErrorClass::DefinitelyNotSent;
        }
    }
    // Everything else (timeout, transport, unknown) is ambiguous.
    ErrorClass::Ambiguous
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
        count = plan.grid.count,
        gas_budget,
        "submitting roll"
    );

    // 1. codegen + 2. compile the per-roll option-coin package.
    let label = format!(
        "{}-{}@{}",
        plan.underlying_symbol, plan.settlement_symbol, plan.expiry_ms
    );
    let pkg = codegen::generate(plan.grid.count, plan.underlying_decimals, &label)
        .context("generating coin package")?;
    let compiled = BuildConfig::new_for_testing()
        .build(&pkg.dir)
        .with_context(|| {
            format!("compiling generated coin package at {}", pkg.dir.display())
        });
    // Always clean the temp dir, success or failure.
    let compiled = match compiled {
        Ok(c) => c,
        Err(e) => {
            pkg.cleanup();
            return Err(e);
        }
    };
    let modules = compiled.get_package_bytes(/* with_unpublished_deps */ false);
    let deps = compiled.get_dependency_storage_package_ids();
    pkg.cleanup();

    // 3. publish + harvest the TreasuryCaps.
    let published =
        coin_pkg::publish_coin_package(&wrap.client, &wrap.signer, modules, deps, gas_budget)
            .await
            .context("publishing coin package")?;
    info!(
        coin_package = %published.package_id,
        digest = %published.digest,
        caps = published.caps.len(),
        "coin package published"
    );

    // 4. pair each cap to its strike and create the buckets.
    let specs = pair_caps_to_strikes(plan, &published.caps)?;
    let call_types: Vec<String> = specs.iter().map(|s| s.call_type.clone()).collect();
    let resp = coin_pkg::create_buckets(
        &wrap.client,
        &wrap.signer,
        package,
        admin_cap,
        &specs,
        gas_budget,
    )
    .await
    .context("creating buckets")?;

    let digest = resp.digest.to_string();
    let bucket_ids = extract_bucket_ids(resp.object_changes.as_deref().unwrap_or(&[]));
    if bucket_ids.is_empty() {
        warn!(
            digest,
            "create_buckets succeeded but no Bucket objects observed in ObjectChanges — \
             relying on indexer to fill in"
        );
    }
    info!(digest, bucket_count = bucket_ids.len(), "roll submitted");
    Ok(RollOutcome { digest, bucket_ids, call_types })
}

/// Pair each harvested `TreasuryCap` to the strike its module was generated
/// for. The cap's call type encodes the module index (`…::call_<i>::CALL_<I>`),
/// which maps to `start_strike + i * strike_interval`.
fn pair_caps_to_strikes(
    plan: &RollPlan,
    caps: &[coin_pkg::HarvestedCap],
) -> Result<Vec<CreateBucketSpec>> {
    if caps.len() as u64 != plan.grid.count {
        anyhow::bail!(
            "harvested {} treasury caps but grid wanted {} buckets",
            caps.len(),
            plan.grid.count
        );
    }
    let mut specs = Vec::with_capacity(caps.len());
    for cap in caps {
        let i = codegen::call_index(&cap.call_type)
            .with_context(|| format!("indexing cap {}", cap.call_type))?;
        if i >= plan.grid.count {
            anyhow::bail!(
                "cap index {i} out of range for grid count {}",
                plan.grid.count
            );
        }
        let strike = plan.grid.start_strike + (i as u128) * plan.grid.strike_interval;
        specs.push(CreateBucketSpec {
            underlying_type: plan.underlying_type.clone(),
            settlement_type: plan.settlement_type.clone(),
            call_type: cap.call_type.clone(),
            cap_ref: cap.cap_ref,
            expiry_ms: plan.expiry_ms,
            strike,
            strike_scale: plan.grid.strike_scale,
        });
    }
    Ok(specs)
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
        // Same module, wrong type name (Position, CallOptionToken) and
        // wrong module entirely (account::Account, sui::SUI) all get
        // dropped.
        let changes = vec![
            created(ObjectID::random(), "bucket", "Position"),
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
            created(ObjectID::random(), "bucket", "Position"),
            mutated(ObjectID::random(), "bucket", "Bucket"),
        ];
        assert_eq!(extract_bucket_ids(&changes), vec![bucket_id]);
    }

    // ── ErrorClass tests ────────────────────────────────────────────

    #[test]
    fn classify_build_error_as_definitely_not_sent() {
        let err = anyhow::anyhow!("building bucket::new_call_option tx: object not found");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_gas_error_as_definitely_not_sent() {
        let err = anyhow::anyhow!("insufficient gas: balance 0, needed 10000");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_signature_error_as_definitely_not_sent() {
        let err = anyhow::anyhow!("signature verification failed");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_revert_as_definitely_not_sent() {
        let err = anyhow::anyhow!("bucket::new_call_option reverted: MoveAbort(...)");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_timeout_as_ambiguous() {
        let err = anyhow::anyhow!("submitting bucket::new_call_option tx: connection timed out");
        assert_eq!(classify_error(&err), ErrorClass::Ambiguous);
    }

    #[test]
    fn classify_unknown_error_as_ambiguous() {
        let err = anyhow::anyhow!("something completely unexpected happened");
        assert_eq!(classify_error(&err), ErrorClass::Ambiguous);
    }
}
