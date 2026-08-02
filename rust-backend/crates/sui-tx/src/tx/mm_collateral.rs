//! Reads against a market maker's own `mm_collateral::CollateralAccount`
//! (the per-MM external collateral package — see
//! docs/audit-restructure/04-collateral-abstraction-plan.md §4). Core holds
//! no MM funds, so balance checks target the MM's own package.

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use std::str::FromStr;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use crate::chain::{return_value_bytes, ChainClient};

/// Read `mm_collateral::balance_of<T>(account)` via devInspect — no tx is
/// submitted and no gas is spent. Returns 0 when the CollateralAccount holds
/// no balance of `T` (the Move view returns 0 for a missing dynamic field).
pub async fn balance_of(
    client: &ChainClient,
    sender: SuiAddress,
    collateral_package: ObjectID,
    account_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let account = pt.obj(crate::tx::shared_object_arg(client, account_id, false).await?)?;
    let coin_tag = TypeTag::from_str(coin_type)
        .with_context(|| format!("parsing coin type {coin_type}"))?;
    pt.programmable_move_call(
        collateral_package,
        Identifier::new("mm_collateral").unwrap(),
        Identifier::new("balance_of").unwrap(),
        vec![coin_tag],
        vec![account],
    );

    let resp = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("devInspect mm_collateral::balance_of")?;

    let bytes = return_value_bytes(&resp, 0)
        .context("devInspect balance_of: missing return value")?;
    let value: u64 = bcs::from_bytes(bytes).context("decoding balance_of u64 return")?;
    Ok(value)
}
