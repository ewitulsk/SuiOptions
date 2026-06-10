//! PTB builders for the protocol's entry points.
//!
//! Two layers here:
//!
//! - Simple Move calls (admin operations, account create) use the high-level
//!   `client.transaction_builder().move_call(...)` API. Everything's a
//!   primitive or an object id, so JSON-encoded args work fine. See
//!   [`admin`] and [`account`].
//!
//! - `execute_write` needs to splice a fresh `SignedQuote` value (built from
//!   `quote::new_quote` + `quote::new_signed_quote`), split a coin from gas,
//!   call `coin::zero<S>` for the empty side, and pass a `FlowKind` enum —
//!   none of that fits the high-level builder. We drop down to
//!   `ProgrammableTransactionBuilder` for that one. See [`execute_write`].

pub mod account;
pub mod admin;
pub mod coin_pkg;
pub mod deepbook;
pub mod execute_write;
pub mod org;
pub mod sponsor;
pub mod template;
pub mod test_tokens;

use anyhow::{anyhow, Result};
use sui_types::base_types::ObjectID;
use sui_types::object::Owner;
use sui_types::transaction::{ObjectArg, SharedObjectMutability};
use tracing::{debug, trace};

use sui_sdk::SuiClient;
use sui_json_rpc_types::SuiObjectDataOptions;

/// Build a `SharedObject` `ObjectArg` from a current chain read. Needed by
/// PTBs that mutate shared objects (Bucket, ProtocolConfig, Treasury,
/// Account, Clock).
pub async fn shared_object_arg(
    client: &SuiClient,
    id: ObjectID,
    mutable: bool,
) -> Result<ObjectArg> {
    trace!(%id, mutable, "fetching shared object arg");
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_owner())
        .await?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("object {id} not found on chain"))?;
    let owner = data
        .owner
        .ok_or_else(|| anyhow!("object {id} has no owner field"))?;
    match owner {
        Owner::Shared {
            initial_shared_version,
        } => Ok(ObjectArg::SharedObject {
            id,
            initial_shared_version,
            mutability: if mutable {
                SharedObjectMutability::Mutable
            } else {
                SharedObjectMutability::Immutable
            },
        }),
        other => Err(anyhow!("object {id} is not shared: {:?}", other)),
    }
}

/// Build an owned-object `ObjectArg` (e.g. an AdminCap held by the deployer).
pub async fn owned_object_arg(client: &SuiClient, id: ObjectID) -> Result<ObjectArg> {
    debug!(%id, "fetching owned object arg");
    let resp = client
        .read_api()
        .get_object_with_options(
            id,
            SuiObjectDataOptions::new().with_owner().with_bcs(),
        )
        .await?;
    let data = resp.data.ok_or_else(|| anyhow!("object {id} not found"))?;
    Ok(ObjectArg::ImmOrOwnedObject(data.object_ref()))
}
