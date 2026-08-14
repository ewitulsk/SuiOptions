//! Bucket-roll executor.
//!
//! A roll is now a single publish-free PTB (SO-393/SO-394): for every
//! strike, `bucket::create_bucket_any_strike<U, S, D0..D9>` registers the
//! option coin's currency at runtime via `sui::coin_registry` (no OTW coin
//! package, no codegen, no in-process compile, no TreasuryCap harvesting)
//! and shares the bucket; pool creation rides in the same PTB. The
//! scheduler is now just the first — not the only — bucket creator: users
//! create arbitrary strikes through the same permissionless entry point,
//! and the grid demotes to a liquidity-seeding policy.
//!
//! Honors `--dry-run`: the caller logs the args it *would* have sent and
//! never reaches `submit`.

use anyhow::{Context, Result};
use sui_tx::chain::{created_objects, ChangedObject, ExecutedTransaction};
use sui_types::base_types::ObjectID;
use tracing::{debug, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::coin_pkg::{self, AnyStrikeSpec};

/// Which option product a roll creates. Calls publish `call_<i>` coin modules
/// and `bucket::create_bucket`; puts publish `put_<i>` modules and
/// `put_bucket::create_put_bucket`. Defaults to `Call` so existing configs and
/// rows (pre-puts) keep their behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProductType {
    Call,
    Put,
}

impl Default for ProductType {
    fn default() -> Self {
        Self::Call
    }
}

impl ProductType {
    /// Lowercase wire/DB tag (`"call"` / `"put"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Put => "put",
        }
    }
}

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
    /// Explicit per-bucket strikes, ascending, in scaled chain units. The
    /// percent grid expands via `StrikeGrid::strikes()`; the z-ladder is
    /// non-uniform by design.
    pub strikes: Vec<u128>,
    pub strike_scale: u8,
    /// Call vs cash-secured put. Selects the codegen coin modules, the
    /// on-chain `create_bucket` vs `create_put_bucket` entry, and the
    /// cap→strike index parser.
    pub product_type: ProductType,
}

impl RollPlan {
    pub fn log_intent(&self, dry_run: bool) {
        info!(
            pair = %format!("{}/{}", self.underlying_symbol, self.settlement_symbol),
            expiry_ms = self.expiry_ms,
            strikes = ?self.strikes,
            count = self.strikes.len(),
            strike_scale = self.strike_scale,
            dry_run,
            "rolling new bucket family"
        );
    }
}

pub struct RollOutcome {
    pub digest: String,
    pub bucket_ids: Vec<ObjectID>,
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
    // Validators refuse to admit a transaction built on a stale gas
    // reference, so nothing executed. `submit_ptb` already retries this with
    // a fresh reference; reaching here means the retry budget ran out, and
    // the slot must be released so the next tick can re-roll rather than
    // parking a family that provably never landed (SO-344).
    if sui_tx::tx::is_stale_gas_rejection(err) {
        return ErrorClass::DefinitelyNotSent;
    }
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
        "building put_bucket::create_put_bucket tx",
    ];
    for pat in &definitely_not_sent {
        if msg.contains(pat) {
            return ErrorClass::DefinitelyNotSent;
        }
    }
    // Everything else (timeout, transport, unknown) is ambiguous.
    ErrorClass::Ambiguous
}

/// Trading-vault pool-allowlisting context (SO-292): when configured,
/// every pool a roll creates is immediately vetted for vault curators so
/// the admin allowlist never goes stale across rolls.
pub struct VaultAllowlist {
    pub adapter_pkg: ObjectID,
    pub allowlist_id: ObjectID,
    pub admin_cap: ObjectID,
}

pub async fn submit(
    wrap: &SuiClientWrapper,
    ctx: &coin_pkg::AnyStrikeContext,
    plan: &RollPlan,
    pools: Option<&coin_pkg::PoolCreation>,
    vault_allowlist: Option<&VaultAllowlist>,
    gas_budget: u64,
) -> Result<RollOutcome> {
    debug!(
        package = %ctx.package,
        bucket_registry = %ctx.bucket_registry,
        underlying = %plan.underlying_type,
        settlement = %plan.settlement_type,
        expiry_ms = plan.expiry_ms,
        count = plan.strikes.len(),
        gas_budget,
        "submitting roll"
    );

    // One publish-free PTB: create + share every strike's bucket (currency
    // registered at runtime inside the call), plus pools when configured.
    let specs: Vec<AnyStrikeSpec> = plan
        .strikes
        .iter()
        .map(|&strike| AnyStrikeSpec {
            underlying_type: plan.underlying_type.clone(),
            settlement_type: plan.settlement_type.clone(),
            expiry_ms: plan.expiry_ms,
            strike,
            strike_scale: plan.strike_scale,
            decimals: plan.underlying_decimals,
            is_put: plan.product_type == ProductType::Put,
        })
        .collect();
    let resp = coin_pkg::create_any_strike_buckets(
        &wrap.client,
        &wrap.signer,
        ctx,
        &specs,
        pools,
        gas_budget,
    )
    .await
    .context("creating buckets")?;

    let digest = sui_tx::tx::tx_digest(&resp).to_string();
    let bucket_ids = extract_bucket_ids(&resp);
    if bucket_ids.is_empty() {
        warn!(
            digest,
            "create_buckets succeeded but no Bucket objects observed in ObjectChanges — \
             relying on indexer to fill in"
        );
    }
    info!(digest, bucket_count = bucket_ids.len(), "roll submitted");

    // Vet the freshly created pools for trading-vault curators. Best
    // effort: a failure here never fails the roll (the admin PTB can be
    // re-run by hand), it just alerts.
    if let Some(va) = vault_allowlist {
        let pool_ids = extract_pool_ids(&resp);
        if !pool_ids.is_empty() {
            if let Err(e) = allowlist_pools(wrap, va, &pool_ids, gas_budget).await {
                tracing::error!(
                    alert_id = "tx-failed-scheduler",
                    error = %format!("{e:#}"),
                    pools = pool_ids.len(),
                    "roll pools created but vault allowlisting failed — run allow_pool by hand"
                );
            } else {
                info!(pools = pool_ids.len(), "roll pools vetted for trading vaults");
            }
        }
    }
    Ok(RollOutcome { digest, bucket_ids })
}

pub(crate) fn extract_pool_ids(resp: &ExecutedTransaction) -> Vec<ObjectID> {
    created_of(&created_objects(resp), "pool", "Pool")
}

/// Ids of `changes` whose type is `<pkg>::<module>::<name>`, in order.
///
/// `changes` is already the CREATED subset — `created_objects` does that
/// filtering, so a mutated Bucket from a later `execute_write` can never
/// reach here and be mistaken for a freshly-rolled one.
fn created_of(changes: &[ChangedObject], module: &str, name: &str) -> Vec<ObjectID> {
    changes
        .iter()
        .filter_map(|c| {
            let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
            (tag.module.as_str() == module && tag.name.as_str() == name).then_some(c.object_id)
        })
        .collect()
}

async fn allowlist_pools(
    wrap: &SuiClientWrapper,
    va: &VaultAllowlist,
    pool_ids: &[ObjectID],
    gas_budget: u64,
) -> Result<()> {
    sui_tx::tx::deepbook::allow_pools_for_vault(
        &wrap.client,
        &wrap.signer,
        va.adapter_pkg,
        va.admin_cap,
        va.allowlist_id,
        pool_ids,
        gas_budget,
    )
    .await
}

/// Pull `bucket::Bucket` ObjectIDs out of a tx's ObjectChanges, in the
/// order they appear. The chain emits one Created per strike for a
/// successful `new_call_option`, so the result lines up with the strike
/// grid the planner submitted.
pub(crate) fn extract_bucket_ids(resp: &ExecutedTransaction) -> Vec<ObjectID> {
    debug!("extracting bucket ids from object changes");
    let mut ids = created_of(&created_objects(resp), "bucket", "Bucket");
    ids.extend(created_of(&created_objects(resp), "put_bucket", "PutBucket"));
    ids
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

    fn created(id: ObjectID, module: &str, name: &str) -> ChangedObject {
        ChangedObject {
            object_id: id,
            object_type: struct_tag(module, name).to_canonical_string(/* with_prefix */ true),
            version: 1,
            digest: String::new(),
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(created_of(&[], "bucket", "Bucket").is_empty());
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
        assert_eq!(created_of(&changes, "bucket", "Bucket"), vec![b1, b2, b3]);
    }

    #[test]
    fn ignores_other_types() {
        // Only the requested module::name is picked up. (Created-vs-mutated
        // filtering happens upstream in `created_objects`, which is what
        // feeds this function.)
        let cap = ObjectID::random();
        let changes = vec![created(cap, "coin", "TreasuryCap")];
        assert!(created_of(&changes, "bucket", "Bucket").is_empty());
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
        assert!(created_of(&changes, "bucket", "Bucket").is_empty());
    }

    #[test]
    fn returns_only_bucket_among_mixed() {
        // The one true Bucket survives, everything else is filtered.
        let bucket_id = ObjectID::random();
        let changes = vec![
            created(ObjectID::random(), "coin", "TreasuryCap"),
            created(bucket_id, "bucket", "Bucket"),
            created(ObjectID::random(), "bucket", "Position"),
        ];
        assert_eq!(created_of(&changes, "bucket", "Bucket"), vec![bucket_id]);
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

    // Verbatim from the rejection that stranded staging: parking this as
    // ambiguous held the roll slot, and the held slot suppressed every
    // later tick for nine days (SO-344).
    #[test]
    fn classify_stale_gas_rejection_as_definitely_not_sent() {
        let err = anyhow::anyhow!(
            "creating buckets: submitting create_buckets tx: gRPC ExecuteTransaction: \
             code: 'Client specified an invalid argument', message: \"Transaction is rejected \
             as invalid by more than 1/3 of validators by stake (non-retriable). \
             Non-retriable errors: [Transaction needs to be rebuilt because object \
             0x49f13ae2 version 0x396fa526 is unavailable for consumption, \
             current version: 0x396fa527]\""
        );
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }
}
