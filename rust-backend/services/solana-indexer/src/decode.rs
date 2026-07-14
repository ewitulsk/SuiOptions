//! Transaction → events extraction.
//!
//! All three programs emit exclusively via `emit_cpi!`: the event bytes are
//! the instruction data of a self-CPI inner instruction —
//! `[8-byte event-ix tag][8-byte event discriminator][Borsh payload]`.
//! Given a Yellowstone `SubscribeUpdateTransactionInfo` we walk every inner
//! instruction, resolve its program id against the static account list
//! (message keys + address-lookup-table loads), and decode the ones owned
//! by our programs. Pure function of the proto message — unit-testable
//! without a stream.

use std::collections::HashMap;

use helius_laserstream::grpc::SubscribeUpdateTransactionInfo;
use tracing::error;

use crate::db::PendingEvent;
use crate::events::{decode_event, Program, EVENT_IX_TAG_LE};

/// The three deployed program ids (base58 → kind), from config.
#[derive(Debug, Clone)]
pub struct ProgramSet {
    by_key: HashMap<Vec<u8>, Program>,
}

impl ProgramSet {
    pub fn new(entries: impl IntoIterator<Item = (crate::events::Pubkey, Program)>) -> Self {
        Self {
            by_key: entries
                .into_iter()
                .map(|(pk, prog)| (pk.0.to_vec(), prog))
                .collect(),
        }
    }

    fn get(&self, key: &[u8]) -> Option<Program> {
        self.by_key.get(key).copied()
    }
}

/// Decode every protocol event in one transaction. Decode failures on a
/// MATCHED discriminator are schema drift — logged loudly (alert) and
/// skipped so third-party garbage can never stall ingestion.
pub fn extract_events(
    info: &SubscribeUpdateTransactionInfo,
    programs: &ProgramSet,
) -> Vec<PendingEvent> {
    let signature = bs58::encode(&info.signature).into_string();
    let Some(meta) = &info.meta else {
        return vec![];
    };
    let Some(tx) = &info.transaction else {
        return vec![];
    };
    let Some(message) = &tx.message else {
        return vec![];
    };

    // Static keys first, then ALT-loaded writable + readonly — the order
    // Solana uses for indices into the combined account list.
    let mut keys: Vec<&[u8]> = message.account_keys.iter().map(|k| k.as_slice()).collect();
    keys.extend(meta.loaded_writable_addresses.iter().map(|k| k.as_slice()));
    keys.extend(meta.loaded_readonly_addresses.iter().map(|k| k.as_slice()));

    let mut out = Vec::new();
    // Global enumeration index across all inner-instruction groups: with
    // the signature it forms the idempotency key, so it must be a stable
    // property of the transaction itself.
    let mut global_ix = 0i32;
    for group in &meta.inner_instructions {
        for ix in &group.instructions {
            let idx = global_ix;
            global_ix += 1;

            let Some(program) = keys
                .get(ix.program_id_index as usize)
                .and_then(|k| programs.get(k))
            else {
                continue;
            };
            if ix.data.len() < 16 || ix.data[..8] != EVENT_IX_TAG_LE {
                continue;
            }
            match decode_event(program, &ix.data[8..]) {
                Ok(Some(event)) => out.push(PendingEvent {
                    event,
                    signature: signature.clone(),
                    tx_index: info.index as i64,
                    inner_ix_index: idx,
                }),
                // Unknown discriminator under our program id: not ours
                // (e.g. a future event this build predates) — skip quietly.
                Ok(None) => {}
                Err(e) => {
                    error!(
                        alert_id = "solana-indexer-decode-failed",
                        error = %e,
                        signature = %signature,
                        program = program.as_str(),
                        inner_ix = idx,
                        "borsh decode of a matched event discriminator failed — schema drift between \
                         the deployed program and this build's event mirrors?"
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{event_discriminator, DecodedEvent, Pubkey};
    use helius_laserstream::solana::storage::confirmed_block::{
        InnerInstruction, InnerInstructions, Message, Transaction, TransactionStatusMeta,
    };

    const CORE_ID: [u8; 32] = [7; 32];

    fn programs() -> ProgramSet {
        ProgramSet::new([(Pubkey(CORE_ID), Program::Core)])
    }

    fn exercised_ix_data() -> Vec<u8> {
        // Exercised { bucket, exerciser, amount, settlement_paid, cursor_after }
        let mut data = EVENT_IX_TAG_LE.to_vec();
        data.extend_from_slice(&event_discriminator("Exercised"));
        data.extend_from_slice(&[1; 32]);
        data.extend_from_slice(&[2; 32]);
        data.extend_from_slice(&5u64.to_le_bytes());
        data.extend_from_slice(&500u64.to_le_bytes());
        data.extend_from_slice(&105u128.to_le_bytes());
        data
    }

    fn tx_info(
        inner: Vec<InnerInstructions>,
        account_keys: Vec<Vec<u8>>,
    ) -> SubscribeUpdateTransactionInfo {
        SubscribeUpdateTransactionInfo {
            signature: vec![9; 64],
            is_vote: false,
            transaction: Some(Transaction {
                signatures: vec![vec![9; 64]],
                message: Some(Message {
                    header: None,
                    account_keys,
                    recent_blockhash: vec![],
                    instructions: vec![],
                    versioned: false,
                    address_table_lookups: vec![],
                }),
            }),
            meta: Some(TransactionStatusMeta {
                inner_instructions: inner,
                ..Default::default()
            }),
            index: 3,
        }
    }

    #[test]
    fn extracts_an_emit_cpi_event() {
        let inner = vec![InnerInstructions {
            index: 0,
            instructions: vec![
                // A foreign inner ix first, so the event lands at global index 1.
                InnerInstruction {
                    program_id_index: 1,
                    accounts: vec![],
                    data: vec![1, 2, 3],
                    stack_height: Some(2),
                },
                InnerInstruction {
                    program_id_index: 0,
                    accounts: vec![],
                    data: exercised_ix_data(),
                    stack_height: Some(2),
                },
            ],
        }];
        let info = tx_info(inner, vec![CORE_ID.to_vec(), [8; 32].to_vec()]);
        let events = extract_events(&info, &programs());
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.inner_ix_index, 1);
        assert_eq!(ev.tx_index, 3);
        assert_eq!(ev.signature, bs58::encode(vec![9; 64]).into_string());
        let DecodedEvent::Exercised(x) = &ev.event else {
            panic!("wrong variant {:?}", ev.event);
        };
        assert_eq!(x.amount.0, 5);
        assert_eq!(x.cursor_after.0, 105);
    }

    #[test]
    fn resolves_program_ids_loaded_via_lookup_tables() {
        let inner = vec![InnerInstructions {
            index: 0,
            instructions: vec![InnerInstruction {
                // Index 1 = first loaded_writable address (static list has 1 key).
                program_id_index: 1,
                accounts: vec![],
                data: exercised_ix_data(),
                stack_height: Some(2),
            }],
        }];
        let mut info = tx_info(inner, vec![[8; 32].to_vec()]);
        info.meta.as_mut().unwrap().loaded_writable_addresses = vec![CORE_ID.to_vec()];
        let events = extract_events(&info, &programs());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn foreign_programs_and_non_event_data_are_skipped() {
        let inner = vec![InnerInstructions {
            index: 0,
            instructions: vec![
                // Our program, but not event-tagged data.
                InnerInstruction {
                    program_id_index: 0,
                    accounts: vec![],
                    data: vec![0; 32],
                    stack_height: Some(2),
                },
                // Event-tagged data under a foreign program.
                InnerInstruction {
                    program_id_index: 1,
                    accounts: vec![],
                    data: exercised_ix_data(),
                    stack_height: Some(2),
                },
            ],
        }];
        let info = tx_info(inner, vec![CORE_ID.to_vec(), [8; 32].to_vec()]);
        assert!(extract_events(&info, &programs()).is_empty());
    }
}
