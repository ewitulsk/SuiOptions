//! Sponsored-transaction validation + fee-payer co-signing — the port of
//! sui-tx's `tx::sponsor` for the Solana fee-payer model.
//!
//! The checks run in this order (each maps to an HTTP status in
//! `handlers.rs`):
//! 1. decode the base64 `VersionedTransaction` (400 on garbage);
//! 2. structural guards — no address lookup tables, fee payer (static
//!    account 0) is the station key, and the station key appears nowhere
//!    else in the message (422);
//! 3. template match against the sponsored-flow shapes (422, with a
//!    human-readable instruction dump — `describe_instructions`);
//! 4. `simulateTransaction` (sigVerify off, replaceRecentBlockhash on):
//!    the tx must succeed, and the station's simulated lamport delta
//!    (fee + rent debits) must stay under the per-tx cap (422; 502 when
//!    the RPC itself fails; 503 when the station balance is already
//!    below the health threshold);
//! 5. sign the message with the station keypair into the fee-payer
//!    signature slot.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::VersionedTransaction;
use tracing::info;

use crate::template::{describe_instructions, match_any, IxView, TxTemplate};

/// A sponsorship failure, tagged with the HTTP class it maps to.
#[derive(Debug)]
pub enum SponsorError {
    /// 400 — the request body is not a decodable transaction.
    BadRequest(String),
    /// 422 — policy refusal; permanent for this transaction shape.
    Policy(String),
    /// 503 — the station balance is below the health threshold.
    LowBalance(String),
    /// 502 — the RPC upstream failed; retryable.
    Upstream(String),
}

impl std::fmt::Display for SponsorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Policy(m) | Self::LowBalance(m) | Self::Upstream(m) => {
                f.write_str(m)
            }
        }
    }
}

/// Result of sponsoring: the co-signed transaction plus the detached
/// station signature.
pub struct SponsoredTx {
    /// base64(serialized VersionedTransaction) with the station signature
    /// in the fee-payer slot — the wallet signs these exact bytes next.
    pub transaction_b64: String,
    /// base58 station signature over the message.
    pub sponsor_signature_b58: String,
    /// Simulated station lamport delta (fee + rent debits).
    pub lamport_delta: u64,
    /// Name of the matched template.
    pub template: String,
}

/// Decode a base64 `VersionedTransaction` (Solana's bincode wire form —
/// legacy and v0 messages both).
pub fn decode_transaction(b64: &str) -> Result<VersionedTransaction, SponsorError> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| SponsorError::BadRequest("transaction is not base64".into()))?;
    bincode::deserialize(&bytes)
        .map_err(|e| SponsorError::BadRequest(format!("transaction does not decode: {e}")))
}

/// Resolve each compiled instruction's program id + data against the
/// static account keys.
fn ix_views(message: &VersionedMessage) -> Result<Vec<IxView>, SponsorError> {
    let keys = message.static_account_keys();
    message
        .instructions()
        .iter()
        .map(|ix| {
            let program = *keys.get(ix.program_id_index as usize).ok_or_else(|| {
                SponsorError::BadRequest("instruction program index out of bounds".into())
            })?;
            Ok(IxView {
                program,
                data: ix.data.clone(),
            })
        })
        .collect()
}

/// Structural guards + template match (checks 2–3 above). Returns the
/// matched template's name.
pub fn validate_transaction<'t>(
    tx: &VersionedTransaction,
    station: &Pubkey,
    templates: &'t [TxTemplate],
) -> Result<&'t str, SponsorError> {
    // v1 rejects lookup-table transactions outright: LUT-resolved accounts
    // are invisible to static inspection, so a LUT could smuggle the
    // station key (or anything else) past the guards below.
    if let VersionedMessage::V0(m) = &tx.message {
        if !m.address_table_lookups.is_empty() {
            return Err(SponsorError::Policy("lookup tables unsupported".into()));
        }
    }

    let keys = tx.message.static_account_keys();
    let Some(fee_payer) = keys.first() else {
        return Err(SponsorError::BadRequest(
            "transaction has no account keys".into(),
        ));
    };
    if fee_payer != station {
        return Err(SponsorError::Policy(format!(
            "fee payer must be the station key {station}, got {fee_payer}"
        )));
    }
    // The station key may appear ONLY as the fee payer (account 0): never
    // duplicated at another index, never referenced by any instruction's
    // account list, never as a program id. Account 0 is always
    // writable+signer, but nothing can move its lamports unless an
    // instruction names it — so excluding it from every account list
    // bounds the exposure to fee + rent, which the simulation cap checks.
    if keys.iter().skip(1).any(|k| k == station) {
        return Err(SponsorError::Policy(
            "station key appears more than once in the account keys".into(),
        ));
    }
    for ix in tx.message.instructions() {
        if ix.program_id_index == 0 {
            return Err(SponsorError::Policy(
                "station key used as a program id".into(),
            ));
        }
        if ix.accounts.contains(&0) {
            return Err(SponsorError::Policy(
                "station key may only be the fee payer; an instruction references it".into(),
            ));
        }
    }

    let ixs = ix_views(&tx.message)?;
    match match_any(templates, &ixs) {
        Some(name) => Ok(name),
        // Include the decoded instruction sequence so a refusal can be
        // diffed against the frontend builders without a redeploy.
        None => Err(SponsorError::Policy(format!(
            "transaction matches no sponsored template: [{}]",
            describe_instructions(&ixs)
        ))),
    }
}

/// Check 4's decision, factored pure for testing against fixture
/// simulation results: the simulation must have succeeded, and the
/// station's lamport delta (pre − post, from the `accounts` config where
/// `addresses = [station]`) must not exceed the per-tx cap.
pub fn lamport_cap_decision(
    pre_lamports: u64,
    sim: &RpcSimulateTransactionResult,
    max_sponsor_lamports_per_tx: u64,
) -> Result<u64, SponsorError> {
    if let Some(err) = &sim.err {
        return Err(SponsorError::Policy(format!(
            "transaction would fail on chain: {err:?}; logs: {:?}",
            sim.logs
        )));
    }
    let post = sim
        .accounts
        .as_ref()
        .and_then(|a| a.first())
        .and_then(|a| a.as_ref())
        .map(|a| a.lamports)
        .ok_or_else(|| {
            SponsorError::Upstream("simulation returned no station account state".into())
        })?;
    let delta = pre_lamports.saturating_sub(post);
    if delta > max_sponsor_lamports_per_tx {
        return Err(SponsorError::Policy(format!(
            "simulated station lamport delta {delta} exceeds the per-tx cap \
             {max_sponsor_lamports_per_tx}"
        )));
    }
    Ok(delta)
}

/// Sign the message with the station key into the fee-payer signature
/// slot (index 0), preserving any signatures the wallet already applied.
pub fn sign_as_sponsor(tx: &mut VersionedTransaction, station: &Keypair) -> Signature {
    let message_bytes = tx.message.serialize();
    let sig = station.sign_message(&message_bytes);
    let required = tx.message.header().num_required_signatures as usize;
    tx.signatures.resize(required.max(1), Signature::default());
    tx.signatures[0] = sig;
    sig
}

/// Sponsorship policy knobs (from config).
pub struct SponsorPolicy {
    pub max_sponsor_lamports_per_tx: u64,
    pub min_balance_threshold_lamports: u64,
}

/// Full sponsorship pipeline (checks 1–5).
pub async fn sponsor_transaction(
    client: &RpcClient,
    station: &Keypair,
    templates: &[TxTemplate],
    policy: &SponsorPolicy,
    transaction_b64: &str,
) -> Result<SponsoredTx, SponsorError> {
    let mut tx = decode_transaction(transaction_b64)?;
    let station_pubkey = station.pubkey();
    let template = validate_transaction(&tx, &station_pubkey, templates)?.to_owned();

    let pre = client
        .get_balance(&station_pubkey)
        .await
        .map_err(|e| SponsorError::Upstream(format!("fetching station balance: {e}")))?;
    if pre < policy.min_balance_threshold_lamports {
        return Err(SponsorError::LowBalance(format!(
            "station balance {pre} lamports is below the threshold {}",
            policy.min_balance_threshold_lamports
        )));
    }

    // sigVerify off (the user signature may not be attached yet) +
    // replaceRecentBlockhash on (the client's blockhash may already be a
    // slot or two old — expiry is enforced at submission, not here).
    let sim = client
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: None,
                    addresses: vec![station_pubkey.to_string()],
                }),
                ..RpcSimulateTransactionConfig::default()
            },
        )
        .await
        .map_err(|e| SponsorError::Upstream(format!("simulateTransaction failed: {e}")))?;
    let lamport_delta =
        lamport_cap_decision(pre, &sim.value, policy.max_sponsor_lamports_per_tx)?;

    let sig = sign_as_sponsor(&mut tx, station);
    let bytes = bincode::serialize(&tx)
        .map_err(|e| SponsorError::Upstream(format!("re-serializing transaction: {e}")))?;
    use base64::Engine as _;
    info!(template, lamport_delta, "sponsored transaction signed");
    Ok(SponsoredTx {
        transaction_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        sponsor_signature_b58: sig.to_string(),
        lamport_delta,
        template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::protocol_templates;
    use base64::Engine as _;
    use solana_sdk::hash::Hash;
    use solana_sdk::instruction::Instruction;
    use solana_sdk::message::{v0, Message};

    fn templates() -> Vec<TxTemplate> {
        protocol_templates(options_core::ID, auction_venue::ID, options_vault::ID)
    }

    /// A real `exercise` instruction; `exerciser` chosen by the caller so
    /// the station key can be smuggled in.
    fn exercise_ix(exerciser: Pubkey) -> Instruction {
        solana_tx::ix::exercise(
            &solana_tx::ix::Exercise {
                exerciser,
                bucket: Pubkey::new_unique(),
                underlying_mint: Pubkey::new_unique(),
                settlement_mint: Pubkey::new_unique(),
                exerciser_call: Pubkey::new_unique(),
                exerciser_settlement: Pubkey::new_unique(),
                exerciser_underlying: Pubkey::new_unique(),
            },
            5,
        )
    }

    fn legacy_tx(ixs: &[Instruction], payer: &Pubkey) -> VersionedTransaction {
        let msg = Message::new_with_blockhash(ixs, Some(payer), &Hash::default());
        let n = msg.header.num_required_signatures as usize;
        VersionedTransaction {
            signatures: vec![Signature::default(); n],
            message: VersionedMessage::Legacy(msg),
        }
    }

    fn v0_tx(ixs: &[Instruction], payer: &Pubkey) -> VersionedTransaction {
        let msg = v0::Message::try_compile(payer, ixs, &[], Hash::default()).unwrap();
        let n = msg.header.num_required_signatures as usize;
        VersionedTransaction {
            signatures: vec![Signature::default(); n],
            message: VersionedMessage::V0(msg),
        }
    }

    fn b64(tx: &VersionedTransaction) -> String {
        base64::engine::general_purpose::STANDARD.encode(bincode::serialize(tx).unwrap())
    }

    #[test]
    fn legacy_and_v0_round_trip_through_decode() {
        let station = Pubkey::new_unique();
        let ix = exercise_ix(Pubkey::new_unique());

        let legacy = legacy_tx(std::slice::from_ref(&ix), &station);
        let decoded = decode_transaction(&b64(&legacy)).unwrap();
        assert_eq!(decoded, legacy);
        assert!(matches!(decoded.message, VersionedMessage::Legacy(_)));

        let v0 = v0_tx(std::slice::from_ref(&ix), &station);
        let decoded = decode_transaction(&b64(&v0)).unwrap();
        assert_eq!(decoded, v0);
        assert!(matches!(decoded.message, VersionedMessage::V0(_)));

        assert!(matches!(
            decode_transaction("not-base64!!!"),
            Err(SponsorError::BadRequest(_))
        ));
        let garbage = base64::engine::general_purpose::STANDARD.encode([0u8; 3]);
        assert!(matches!(
            decode_transaction(&garbage),
            Err(SponsorError::BadRequest(_))
        ));
    }

    #[test]
    fn valid_exercise_passes_both_message_versions() {
        let station = Pubkey::new_unique();
        let ix = exercise_ix(Pubkey::new_unique());
        let t = templates();
        for tx in [
            legacy_tx(std::slice::from_ref(&ix), &station),
            v0_tx(std::slice::from_ref(&ix), &station),
        ] {
            assert_eq!(
                validate_transaction(&tx, &station, &t).unwrap(),
                "exercise"
            );
        }
    }

    #[test]
    fn wrong_fee_payer_rejected() {
        let station = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let tx = legacy_tx(&[exercise_ix(Pubkey::new_unique())], &other);
        let err = validate_transaction(&tx, &station, &templates()).unwrap_err();
        assert!(matches!(&err, SponsorError::Policy(m) if m.contains("fee payer")), "{err}");
    }

    #[test]
    fn station_key_in_an_instruction_rejected() {
        let station = Pubkey::new_unique();
        // The station key smuggled in as the exerciser: it compiles to
        // static index 0 (fee payer) AND is referenced by the instruction.
        let tx = legacy_tx(&[exercise_ix(station)], &station);
        let err = validate_transaction(&tx, &station, &templates()).unwrap_err();
        assert!(
            matches!(&err, SponsorError::Policy(m) if m.contains("only be the fee payer")),
            "{err}"
        );
    }

    #[test]
    fn lookup_table_transactions_rejected() {
        let station = Pubkey::new_unique();
        let mut tx = v0_tx(&[exercise_ix(Pubkey::new_unique())], &station);
        let VersionedMessage::V0(m) = &mut tx.message else {
            unreachable!()
        };
        m.address_table_lookups.push(v0::MessageAddressTableLookup {
            account_key: Pubkey::new_unique(),
            writable_indexes: vec![0],
            readonly_indexes: vec![],
        });
        let err = validate_transaction(&tx, &station, &templates()).unwrap_err();
        assert!(
            matches!(&err, SponsorError::Policy(m) if m.contains("lookup tables unsupported")),
            "{err}"
        );
    }

    #[test]
    fn non_template_transaction_rejected_with_instruction_dump() {
        let station = Pubkey::new_unique();
        let foreign = Instruction::new_with_bytes(
            Pubkey::new_unique(),
            &[1u8; 12],
            vec![solana_sdk::instruction::AccountMeta::new(
                Pubkey::new_unique(),
                false,
            )],
        );
        let tx = legacy_tx(&[foreign], &station);
        let err = validate_transaction(&tx, &station, &templates()).unwrap_err();
        assert!(
            matches!(&err, SponsorError::Policy(m) if m.contains("no sponsored template")),
            "{err}"
        );
    }

    // ---------------------------------------------------- lamport cap

    fn sim_fixture(json: serde_json::Value) -> RpcSimulateTransactionResult {
        serde_json::from_value(json).unwrap()
    }

    fn ok_sim(post_lamports: u64) -> RpcSimulateTransactionResult {
        sim_fixture(serde_json::json!({
            "err": null,
            "logs": [],
            "accounts": [{
                "lamports": post_lamports,
                "data": ["", "base64"],
                "owner": "11111111111111111111111111111111",
                "executable": false,
                "rentEpoch": 0,
            }],
            "unitsConsumed": 5000,
        }))
    }

    #[test]
    fn lamport_cap_allows_under_and_refuses_over() {
        // fee 5_000 + ATA rent ~2_039_280 → under the 5e6 cap.
        let delta = lamport_cap_decision(10_000_000_000, &ok_sim(9_997_955_720), 5_000_000)
            .unwrap();
        assert_eq!(delta, 2_044_280);

        // A 0.1 SOL debit blows the cap.
        let err = lamport_cap_decision(10_000_000_000, &ok_sim(9_900_000_000), 5_000_000)
            .unwrap_err();
        assert!(
            matches!(&err, SponsorError::Policy(m) if m.contains("exceeds the per-tx cap")),
            "{err}"
        );

        // A credit (post > pre) is a zero delta, never an underflow.
        let delta =
            lamport_cap_decision(10_000_000_000, &ok_sim(10_000_001_000), 5_000_000).unwrap();
        assert_eq!(delta, 0);
    }

    #[test]
    fn failed_simulation_is_a_policy_refusal() {
        let sim = sim_fixture(serde_json::json!({
            "err": "AccountNotFound",
            "logs": ["Program failed"],
        }));
        let err = lamport_cap_decision(10_000_000_000, &sim, 5_000_000).unwrap_err();
        assert!(
            matches!(&err, SponsorError::Policy(m) if m.contains("would fail on chain")),
            "{err}"
        );
    }

    #[test]
    fn missing_station_account_state_is_upstream() {
        let sim = sim_fixture(serde_json::json!({ "err": null, "logs": [] }));
        let err = lamport_cap_decision(1, &sim, 5_000_000).unwrap_err();
        assert!(matches!(err, SponsorError::Upstream(_)), "{err}");
    }

    // ---------------------------------------------------- signing

    #[test]
    fn sponsor_signature_lands_in_slot_zero_and_verifies() {
        let station = Keypair::new();
        let ix = exercise_ix(Pubkey::new_unique());
        let mut tx = legacy_tx(std::slice::from_ref(&ix), &station.pubkey());

        let sig = sign_as_sponsor(&mut tx, &station);
        assert_eq!(tx.signatures[0], sig);
        assert_eq!(
            tx.signatures.len(),
            tx.message.header().num_required_signatures as usize
        );
        let msg_bytes = tx.message.serialize();
        assert!(sig.verify(station.pubkey().as_ref(), &msg_bytes));

        // Round-trips through the wire form with the signature intact.
        let back = decode_transaction(&b64(&tx)).unwrap();
        assert_eq!(back.signatures[0], sig);
    }
}
