//! Account-creation PTB + on-chain account lookup.
//!
//! `account::create_and_share_account(signing_pubkey: vector<u8>, ctx)`
//! creates the Account, registers its signing pubkey, and shares it. The MM
//! bot calls this once when no account exists yet for the current
//! deployment; [`find_account`] is how it discovers an already-created one
//! (the Account id is a random shared-object UID, so it can't be derived
//! from the key).

use anyhow::{anyhow, Context, Result};
use move_core_types::account_address::AccountAddress;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::StructTag;
use shared_crypto::intent::Intent;
use sui_json::SuiJsonValue;
use sui_json_rpc_types::{
    EventFilter, ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::event::EventID;
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

/// Find this bot's Account on the *current* `package`, if one already exists.
///
/// The Account object id is a random shared-object UID (see
/// `account::create_account`), so it can't be computed from the key. Instead
/// we read it back from chain state: scan `AccountCreated` events emitted by
/// `package` and return the one whose transaction `sender` is `owner` and
/// whose registered `(scheme, pubkey)` match ours. Because the event type is
/// package-qualified, accounts created under a *prior* deployment are
/// invisible here — so this answers exactly "does the current deployment
/// have my account?" and the bot bootstraps a fresh one after a redeploy.
///
/// Returns `None` when no matching account has been created under `package`.
pub async fn find_account(
    client: &SuiClient,
    package: ObjectID,
    owner: SuiAddress,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
) -> Result<Option<ObjectID>> {
    let event_type = StructTag {
        address: AccountAddress::new(package.into_bytes()),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("AccountCreated").unwrap(),
        type_params: vec![],
    };
    let filter = EventFilter::MoveEventType(event_type);

    let mut cursor: Option<EventID> = None;
    loop {
        // Descending (newest first) so a re-bootstrapped owner surfaces its
        // latest account first. `AccountCreated` is emitted once per account
        // (key rotation emits `SigningKeyRotated`), so matches are unique.
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(50), true)
            .await
            .context("querying AccountCreated events")?;

        for ev in &page.data {
            if ev.sender != owner {
                continue;
            }
            let pj = &ev.parsed_json;
            let scheme_ok = pj
                .get("signing_scheme")
                .and_then(|v| v.as_u64())
                .is_some_and(|s| s as u8 == signing_scheme.as_u8());
            let pubkey_ok = pj
                .get("signing_pubkey")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| {
                    arr.len() == signing_pubkey.len()
                        && arr
                            .iter()
                            .zip(signing_pubkey)
                            .all(|(j, b)| j.as_u64() == Some(*b as u64))
                });
            if !(scheme_ok && pubkey_ok) {
                continue;
            }
            let id_str = pj
                .get("account_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("AccountCreated event missing account_id"))?;
            let account_id = ObjectID::from_hex_literal(id_str)
                .with_context(|| format!("parsing account_id {id_str}"))?;
            debug!(%account_id, %owner, "found existing on-chain account");
            return Ok(Some(account_id));
        }

        if !page.has_next_page {
            break;
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(None)
}
