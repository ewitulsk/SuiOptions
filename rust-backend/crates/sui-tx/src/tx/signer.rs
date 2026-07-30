//! QuoteSigner-creation PTB + on-chain signer lookup.
//!
//! `quote_signer::create_and_share_signer(scheme: u8, pubkey: vector<u8>, ctx)`
//! creates the QuoteSigner (signing key + nonce table — core holds no MM
//! funds), registers its signing pubkey, and shares it. The MM bot calls this
//! once when no signer exists yet for the current deployment;
//! [`find_signer`] is how it discovers an already-created one (the signer id
//! is a random shared-object UID, so it can't be derived from the key).

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
use std::str::FromStr;

use sui_types::base_types::{ObjectID, ObjectType, SuiAddress};
use sui_types::error::SuiObjectResponseError;
use sui_types::event::EventID;
use sui_types::transaction::Transaction;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, info};

use crate::sui_client::Signer;

pub struct SignerCreated {
    pub signer_id: ObjectID,
    pub digest: String,
}

/// Calls `quote_signer::create_and_share_signer(scheme, pubkey, ctx)` and
/// returns the shared QuoteSigner's object id.
pub async fn create_and_share_signer(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
    gas_budget: u64,
) -> Result<SignerCreated> {
    info!(%package, scheme = ?signing_scheme, pubkey_len = signing_pubkey.len(), "creating on-chain quote signer");
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
            "quote_signer",
            "create_and_share_signer",
            vec![],
            vec![scheme_arg, SuiJsonValue::new(serde_json::Value::Array(pubkey_array))?],
            None,
            gas_budget,
            None,
        )
        .await
        .context("building create_and_share_signer tx")?;
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
        .context("submitting create_and_share_signer tx")?;

    let effects = resp
        .effects
        .as_ref()
        .context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("create_and_share_signer reverted: {:?}", effects.status());
    }

    // Pull out the QuoteSigner object id from object_changes.
    let changes = resp
        .object_changes
        .as_ref()
        .context("response missing object_changes")?;
    let signer_id = changes
        .iter()
        .find_map(|c| match c {
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "quote_signer"
                && object_type.name.as_str() == "QuoteSigner" =>
            {
                Some(*object_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("QuoteSigner object not found in response"))?;

    debug!(%signer_id, digest = %resp.digest, "quote signer created on-chain");
    Ok(SignerCreated {
        signer_id,
        digest: resp.digest.to_string(),
    })
}

/// Find this bot's QuoteSigner on the *current* `package`, if one already
/// exists.
///
/// The signer object id is a random shared-object UID (see
/// `quote_signer::create_signer`), so it can't be computed from the key.
/// Instead we read it back from chain state: scan `SignerCreated` events
/// emitted by `package` and return the one whose transaction `sender` is
/// `owner` and whose registered `(scheme, pubkey)` match ours. Because the
/// event type is package-qualified, signers created under a *prior*
/// deployment are invisible here — so this answers exactly "does the current
/// deployment have my signer?" and the bot bootstraps a fresh one after a
/// redeploy.
///
/// Returns `None` when no matching signer has been created under `package`.
pub async fn find_signer(
    client: &SuiClient,
    package: ObjectID,
    owner: SuiAddress,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
) -> Result<Option<ObjectID>> {
    let event_type = StructTag {
        address: AccountAddress::new(package.into_bytes()),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("SignerCreated").unwrap(),
        type_params: vec![],
    };
    let filter = EventFilter::MoveEventType(event_type);

    let mut cursor: Option<EventID> = None;
    loop {
        // Descending (newest first) so a re-bootstrapped owner surfaces its
        // latest signer first. `SignerCreated` is emitted once per signer
        // (key rotation emits `SigningKeyRotated`), so matches are unique.
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(50), true)
            .await
            .context("querying SignerCreated events")?;

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
                .get("signer_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("SignerCreated event missing signer_id"))?;
            let signer_id = ObjectID::from_hex_literal(id_str)
                .with_context(|| format!("parsing signer_id {id_str}"))?;
            debug!(%signer_id, %owner, "found existing on-chain quote signer");
            return Ok(Some(signer_id));
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

/// Verify that `signer_id` is a QuoteSigner this bot may adopt, by reading the
/// object rather than replaying history.
///
/// [`find_signer`] answers the same question from `SignerCreated` events, which
/// makes it a hostage to archival retention: the event's transaction can age
/// out of a provider while the object it created is still live and readable.
/// That is not hypothetical — it is why mm-bot could not boot (SO-325). This
/// reads current state instead, and checks strictly more than the type:
///
/// * the object exists and its type is `<package>::quote_signer::QuoteSigner`,
///   so deployment membership is a property of the object — a signer from a
///   prior deployment carries a different package id and is rejected without
///   the event scoping `find_signer` relies on;
/// * `owner`, `signing_scheme` and `signing_pubkey` match ours — the same
///   three predicates `find_signer` matches on, so adopting another bot's
///   signer is rejected here exactly as it is there.
///
/// `Ok(false)` means a definitive "not ours" — absent object, wrong package,
/// or a field mismatch — and the caller should fall back to the normal
/// discovery path. An RPC failure is an `Err`, never a `false`: "I could not
/// tell" must not be read as "no signer exists", because that would bootstrap
/// a duplicate alongside a live one.
pub async fn verify_signer(
    client: &SuiClient,
    package: ObjectID,
    signer_id: ObjectID,
    owner: SuiAddress,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
) -> Result<bool> {
    let resp = client
        .read_api()
        .get_object_with_options(
            signer_id,
            sui_json_rpc_types::SuiObjectDataOptions::new()
                .with_type()
                .with_content(),
        )
        .await
        .context("reading configured quote signer object")?;

    // Only errors that positively answer "this is not an adoptable signer"
    // become `false`. `Unknown` and `DisplayError` say nothing about whether
    // the object exists, so they are errors — treating them as "not ours"
    // would let a transient RPC hiccup reach the bootstrap path once the
    // event fallback below starts returning a legitimate `Ok(None)`.
    if let Some(err) = resp.error {
        return match err {
            SuiObjectResponseError::NotExists { .. }
            | SuiObjectResponseError::Deleted { .. } => {
                debug!(%signer_id, ?err, "configured quote signer is gone — falling back");
                Ok(false)
            }
            other => Err(anyhow!("reading configured quote signer {signer_id}: {other}")),
        };
    }
    let Some(data) = resp.data else {
        debug!(%signer_id, "configured quote signer returned no data — falling back");
        return Ok(false);
    };

    // Compare the parsed type, not a rendered string: Move address formatting
    // varies on leading-zero padding, and a formatting mismatch here would
    // fail verification silently and turn this whole path into a no-op.
    // Matches how `create_and_share_signer` identifies the object above.
    let is_ours = match data.type_.as_ref() {
        Some(ObjectType::Struct(t)) => {
            t.address() == AccountAddress::from(package)
                && t.module().as_str() == "quote_signer"
                && t.name().as_str() == "QuoteSigner"
        }
        _ => false,
    };
    if !is_ours {
        info!(
            %signer_id, actual_type = ?data.type_, %package,
            "configured quote signer is not a QuoteSigner of this deployment — falling back"
        );
        return Ok(false);
    }

    let Some(fields) = data.content.as_ref().and_then(|c| match c {
        sui_json_rpc_types::SuiParsedData::MoveObject(o) => Some(o.fields.clone().to_json_value()),
        _ => None,
    }) else {
        debug!(%signer_id, "configured quote signer has no parsed fields — falling back");
        return Ok(false);
    };

    if !fields_match(&fields, owner, signing_scheme, signing_pubkey) {
        info!(
            %signer_id,
            "configured quote signer is not this bot's — falling back"
        );
        return Ok(false);
    }

    debug!(%signer_id, %owner, "verified configured quote signer by object read");
    Ok(true)
}

/// Do a QuoteSigner's parsed Move fields identify *this* bot's signer?
///
/// Split out from [`verify_signer`] because this is the safety-critical half
/// and the only half that can be tested without a chain: it is what stops the
/// bot adopting a signer belonging to someone else, whose registered pubkey
/// would not match the key it actually signs quotes with. Every check must
/// pass — a missing or unparseable field is a mismatch, never a pass.
fn fields_match(
    fields: &serde_json::Value,
    owner: SuiAddress,
    signing_scheme: protocol_types::SigningScheme,
    signing_pubkey: &[u8],
) -> bool {
    let owner_ok = fields
        .get("owner")
        .and_then(|v| v.as_str())
        .and_then(|s| SuiAddress::from_str(s).ok())
        .is_some_and(|a| a == owner);
    let scheme_ok = fields
        .get("signing_scheme")
        .and_then(json_u64)
        .is_some_and(|s| s as u8 == signing_scheme.as_u8());
    let pubkey_ok = fields
        .get("signing_pubkey")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| {
            arr.len() == signing_pubkey.len()
                && arr.iter().zip(signing_pubkey).all(|(j, b)| json_u64(j) == Some(*b as u64))
        });
    owner_ok && scheme_ok && pubkey_ok
}

/// Move u8/u64 fields arrive as JSON numbers or as decimal strings depending
/// on width; accept both rather than silently failing the comparison.
fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::SigningScheme;
    use serde_json::json;

    const OWNER: &str = "0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865";
    const OTHER: &str = "0x1898ef5fbcddc7ca3d4a9a7495c2531b34c2eca6ffc5ea8a50d545c0000000001";

    fn owner() -> SuiAddress {
        SuiAddress::from_str(OWNER).unwrap()
    }

    /// Shaped like a real `sui_getObject` content payload for a QuoteSigner.
    fn fields(owner: &str, scheme: u64, pubkey: &[u8]) -> serde_json::Value {
        json!({
            "id": { "id": "0xadae1712d2efdf7cf7d7ce16e6bff32c937395e76732aca744d9763b148797dc" },
            "owner": owner,
            "signing_scheme": scheme,
            "signing_pubkey": pubkey.iter().map(|b| json!(b)).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn accepts_this_bots_signer() {
        let pk = [7u8; 32];
        assert!(fields_match(&fields(OWNER, 0, &pk), owner(), SigningScheme::Ed25519, &pk));
    }

    #[test]
    fn rejects_another_bots_signer() {
        // The hazard this function exists for: right deployment, right type,
        // wrong owner. Adopting it would sign with a key the chain does not
        // have registered for us.
        let pk = [7u8; 32];
        assert!(!fields_match(&fields(OTHER, 0, &pk), owner(), SigningScheme::Ed25519, &pk));
    }

    #[test]
    fn rejects_mismatched_pubkey() {
        let ours = [7u8; 32];
        let theirs = [9u8; 32];
        assert!(!fields_match(&fields(OWNER, 0, &theirs), owner(), SigningScheme::Ed25519, &ours));
    }

    #[test]
    fn rejects_truncated_pubkey() {
        let ours = [7u8; 32];
        assert!(!fields_match(&fields(OWNER, 0, &ours[..31]), owner(), SigningScheme::Ed25519, &ours));
    }

    #[test]
    fn rejects_missing_fields() {
        let pk = [7u8; 32];
        for missing in ["owner", "signing_scheme", "signing_pubkey"] {
            let mut f = fields(OWNER, 0, &pk);
            f.as_object_mut().unwrap().remove(missing);
            assert!(
                !fields_match(&f, owner(), SigningScheme::Ed25519, &pk),
                "missing {missing} must not pass"
            );
        }
    }

    #[test]
    fn accepts_stringified_numbers() {
        // Move integers come back as JSON numbers or decimal strings
        // depending on width; a string must not read as a mismatch.
        let pk = [7u8; 32];
        let f = json!({
            "owner": OWNER,
            "signing_scheme": "0",
            "signing_pubkey": pk.iter().map(|b| json!(b.to_string())).collect::<Vec<_>>(),
        });
        assert!(fields_match(&f, owner(), SigningScheme::Ed25519, &pk));
    }
}
