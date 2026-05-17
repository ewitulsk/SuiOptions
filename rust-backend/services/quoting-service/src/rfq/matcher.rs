//! Deadline-bounded collection of MM responses.
//!
//! [`channel`] makes a pair: an [`MatcherInput`] (every connected MM gets a
//! clone of its `tx`; the orchestrator uses `expect()` to declare who it's
//! waiting for) and an [`MatcherOutput`] receiver that yields a final
//! `responses` vec once either the deadline elapses or all expected MMs
//! have answered.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use shared::protocol_types::ids::ObjectId;
use shared::protocol_types::messages::MmQuotePayload;

pub use crate::state::MmResponse;

pub struct MatcherInput {
    pub tx: mpsc::Sender<MmResponse>,
    expected: HashSet<ObjectId>,
}

impl MatcherInput {
    pub fn expect(&mut self, mm: ObjectId) {
        self.expected.insert(mm);
    }
}

#[derive(Debug, Default)]
pub struct MatcherOutput {
    pub responses: Vec<(ObjectId, MmQuotePayload)>,
    pub declines: Vec<ObjectId>,
}

pub fn channel(capacity: usize) -> (MatcherInput, mpsc::Receiver<MmResponse>) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        MatcherInput {
            tx,
            expected: HashSet::new(),
        },
        rx,
    )
}

/// Drain `rx` until either: every expected MM has answered, the sender
/// halves all drop (every MM disconnected mid-RFQ), or `window` elapses.
pub async fn collect_with_deadline(
    mut rx: mpsc::Receiver<MmResponse>,
    window: Duration,
) -> MatcherOutput {
    let mut out = MatcherOutput::default();
    let _ = timeout(window, async {
        while let Some(r) = rx.recv().await {
            match r {
                MmResponse::Quote(mm, q) => out.responses.push((mm, q)),
                MmResponse::Decline(mm) => out.declines.push(mm),
            }
        }
    })
    .await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol_types::ids::SuiAddress;
    use shared::protocol_types::quote::Quote;

    fn fake_payload(nonce: u64, premium: u64) -> MmQuotePayload {
        MmQuotePayload {
            quote: Quote {
                protocol_id: vec![],
                signer_account_id: ObjectId::ZERO,
                signer_token_recipient: SuiAddress::ZERO,
                bucket_id: ObjectId::ZERO,
                write_amount: 1,
                premium,
                valid_until_ms: 999,
                nonce,
            },
            signature: vec![],
        }
    }

    #[tokio::test]
    async fn collects_until_senders_drop() {
        let (input, rx) = channel(4);
        let tx = input.tx.clone();
        let h = tokio::spawn(collect_with_deadline(rx, Duration::from_secs(10)));
        tx.send(MmResponse::Quote(ObjectId::new([0x01; 32]), fake_payload(1, 100)))
            .await
            .unwrap();
        tx.send(MmResponse::Decline(ObjectId::new([0x02; 32])))
            .await
            .unwrap();
        drop(tx);
        drop(input);
        let out = h.await.unwrap();
        assert_eq!(out.responses.len(), 1);
        assert_eq!(out.declines.len(), 1);
    }

    #[tokio::test]
    async fn returns_on_deadline_even_if_senders_open() {
        let (input, rx) = channel(4);
        let _input_keepalive = input; // keep the sender alive so the channel doesn't close
        let out = collect_with_deadline(rx, Duration::from_millis(50)).await;
        assert!(out.responses.is_empty());
        assert!(out.declines.is_empty());
    }
}
