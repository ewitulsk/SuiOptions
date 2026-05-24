//! Account-creation PTB.
//!
//! `account::create_and_share_account(signing_pubkey: vector<u8>, ctx)`
//! creates the Account, registers its signing pubkey, and shares it. The MM
//! bot calls this once on first boot, then persists the resulting
//! `account_id` so subsequent runs reuse it.

use anyhow::{anyhow, Context, Result};
use shared_crypto::intent::Intent;
use sui_json::SuiJsonValue;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::transaction::Transaction;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, info};

use crate::sui_client::Signer;

pub struct AccountCreated {
    pub account_id: ObjectID,
    pub digest: String,
}

/// Calls `account::create_and_share_account(scheme, pubkey, ctx)` and
/// returns the shared Account's object id.
pub async fn create_and_share_account(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
    gas_budget: u64,
) -> Result<AccountCreated> {
    info!(%package, scheme = ?signing_scheme, pubkey_len = signing_pubkey.len(), "creating on-chain account");
    // `vector<u8>` rides as a JSON array of decimal-string-encoded bytes.
    let pubkey_array: Vec<serde_json::Value> = signing_pubkey
        .iter()
        .map(|b| serde_json::Value::Number((*b as u64).into()))
        .collect();
    let scheme_arg = SuiJsonValue::new(serde_json::Value::Number(
        (signing_scheme.as_u8() as u64).into(),
    ))?;

    let tx_data = client
        .transaction_builder()
        .move_call(
            signer.address,
            package,
            "account",
            "create_and_share_account",
            vec![],
            vec![scheme_arg, SuiJsonValue::new(serde_json::Value::Array(pubkey_array))?],
            None,
            gas_budget,
            None,
        )
        .await
        .context("building create_and_share_account tx")?;
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp: SuiTransactionBlockResponse = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting create_and_share_account tx")?;

    let effects = resp
        .effects
        .as_ref()
        .context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("create_and_share_account reverted: {:?}", effects.status());
    }

    // Pull out the Account object id from object_changes.
    let changes = resp
        .object_changes
        .as_ref()
        .context("response missing object_changes")?;
    let account_id = changes
        .iter()
        .find_map(|c| match c {
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "account"
                && object_type.name.as_str() == "Account" =>
            {
                Some(*object_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("Account object not found in response"))?;

    debug!(%account_id, digest = %resp.digest, "account created on-chain");
    Ok(AccountCreated {
        account_id,
        digest: resp.digest.to_string(),
    })
}
