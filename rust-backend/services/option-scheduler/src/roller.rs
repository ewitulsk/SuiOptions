//! Bucket-roll executor.
//!
//! Builds a `NewCallOptionArgs`, submits it via the shared admin tx
//! builder, and pulls every created bucket id out of `ObjectChanges` so
//! we can log them. Honors `--dry-run`: we log the args we *would* have
//! sent and return without touching the chain.

use anyhow::{Context, Result};
use sui_json_rpc_types::{ObjectChange, SuiTransactionBlockResponse};
use sui_types::base_types::ObjectID;
use tracing::{info, warn};

use shared::sui_client::SuiClientWrapper;
use shared::tx::admin::{new_call_option, NewCallOptionArgs};

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
            start_strike = self.grid.start_strike,
            strike_interval = self.grid.strike_interval,
            count = self.grid.count,
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
        },
        gas_budget,
    )
    .await
    .context("submitting new_call_option")?;

    let digest = resp.digest.to_string();
    let bucket_ids = extract_bucket_ids(&resp);
    if bucket_ids.is_empty() {
        warn!(
            digest,
            "new_call_option succeeded but no Bucket objects observed in ObjectChanges — \
             relying on indexer to fill in"
        );
    }
    Ok(RollOutcome { digest, bucket_ids })
}

fn extract_bucket_ids(resp: &SuiTransactionBlockResponse) -> Vec<ObjectID> {
    let Some(changes) = resp.object_changes.as_ref() else {
        return vec![];
    };
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
